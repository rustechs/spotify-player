use std::collections::HashSet;
use std::ops::Deref;
use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::state::Lyrics;
use crate::{auth, config};
use crate::{
    auth::AuthConfig,
    state::{
        store_data_into_file_cache, Album, AlbumId, Artist, ArtistId, Category, Context, ContextId,
        Device, FileCacheKey, Item, ItemId, MemoryCaches, Playback, PlaybackMetadata, Playlist,
        PlaylistFolderItem, PlaylistId, SearchResults, SharedState, Show, ShowId, Track, TrackId,
        UserId, TTL_CACHE_DURATION, USER_LIKED_TRACKS_URI, USER_RECENTLY_PLAYED_TRACKS_URI,
        USER_TOP_TRACKS_LONG_TERM_URI, USER_TOP_TRACKS_SHORT_TERM_URI, USER_TOP_TRACKS_URI,
    },
};

use std::io::Write;

use anyhow::Context as _;
use anyhow::Result;

use librespot_core::SpotifyUri;
#[cfg(feature = "streaming")]
use parking_lot::Mutex;

use reqwest::StatusCode;
use rspotify::{http::Query, prelude::*};

mod handlers;
mod request;
mod spotify;

pub use handlers::*;
pub use request::*;
use serde::Deserialize;
pub(crate) use spotify::WebApiClient;

const SPOTIFY_API_ENDPOINT: &str = "https://api.spotify.com/v1";
const PLAYBACK_TYPES: [&rspotify::model::AdditionalType; 2] = [
    &rspotify::model::AdditionalType::Track,
    &rspotify::model::AdditionalType::Episode,
];

/// Cap on how long we wait to recreate an invalid librespot session.
/// Without this, a hung `session.connect` freezes the TUI (and CLI socket).
const SESSION_RECONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Bound direct `retrieve_current_playback` calls (initialize/update paths) the same way
/// the client-request handler bounds `ClientRequest` work. Without this, a hung Web API
/// call outside the handler can stall a worker indefinitely.
const PLAYBACK_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Default HTTP timeout for the app's direct `reqwest` client (`http_get` helpers).
/// rspotify's client already defaults to 10s; keep this in the same ballpark.
const HTTP_CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound for Spotify `Retry-After` sleeps so a huge header cannot wedge the app.
const MAX_RETRY_AFTER: Duration = Duration::from_mins(1);

/// How long to wait for a woken desktop client to register with Connect.
#[cfg(target_os = "linux")]
const PREFERRED_DEVICE_WAIT: Duration = Duration::from_secs(15);
/// Brief Connect poll when MPRIS is already Playing — do not block startup
/// waiting for a device that may stay invisible to the Web API.
#[cfg(target_os = "linux")]
const ALREADY_PLAYING_DEVICE_WAIT: Duration = Duration::from_secs(3);

/// The application's Spotify client
#[derive(Clone)]
pub struct AppClient {
    http: reqwest::Client,
    /// The integrated Spotify client, mainly used for streaming and librespot integration
    spotify: Arc<spotify::Spotify>,
    auth_config: AuthConfig,
    /// The Spotify Web API client, used for interacting with Spotify Web APIs
    api_client: WebApiClient,
    #[cfg(feature = "streaming")]
    stream_conn: Arc<Mutex<Option<librespot_connect::Spirc>>>,
    /// Serialize session recreation and prevent concurrent hung reconnects.
    session_reconnect: Arc<tokio::sync::Mutex<()>>,
}

impl Deref for AppClient {
    type Target = WebApiClient;
    fn deref(&self) -> &Self::Target {
        &self.api_client
    }
}

/// Build the Spotify Web API client from the configured client ID.
///
/// The returned client is unauthenticated; call [`auth::prompt_for_user_token`] to obtain an
/// access token.
pub fn new_api_client() -> Result<WebApiClient> {
    let configs = config::get_config();

    let id = configs.app_config.get_client_id()?;
    // The bundled default (ncspot's client ID) is registered with extended quota mode and
    // predates Spotify's 2024 Web API changes, so it is far less likely to hit rate limits
    // than a freshly-registered client. Warn users who override it that they may run into
    // `429 Too Many Requests` / `403 Forbidden` errors.
    //
    // See https://github.com/aome510/spotify-player/issues/890 for details.
    if id != auth::NCSPOT_CLIENT_ID {
        tracing::warn!(
            "A custom `client_id` ({id}) is configured. Newly-registered Spotify clients \
             use the restricted default quota mode and may hit rate-limit (429) or \
             forbidden (403) errors. Unless you specifically need your own client, \
             consider removing `client_id`/`client_id_command` to use the bundled default. \
             See https://github.com/aome510/spotify-player/issues/890 for details."
        );
    }

    let creds = rspotify::Credentials { id, secret: None };
    let mut scopes = auth::OAUTH_SCOPES
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    // `user-personalized` scope is not supported by the Web API client and only available to the official Spotify client
    scopes.remove("user-personalized");
    let oauth = rspotify::OAuth {
        redirect_uri: configs.app_config.login_redirect_uri.clone(),
        scopes,
        ..Default::default()
    };
    let config = rspotify::Config {
        token_cached: true,
        cache_path: configs.cache_folder.join("user_client_token.json"),
        ..Default::default()
    };
    Ok(WebApiClient::new(
        rspotify::AuthCodePkceSpotify::with_config(creds, oauth, config),
    ))
}

/// Whether `new_session` should drop in-memory TTL caches.
///
/// `true` only for a fresh login (`reauth`). Reconnect keeps context/search/
/// lyrics/genre/image entries that are still valid for this user.
fn clear_memory_caches_on_new_session(reauth: bool) -> bool {
    reauth
}

fn next_repeat_state(current: rspotify::model::RepeatState) -> rspotify::model::RepeatState {
    use rspotify::model::RepeatState::{Context, Off, Track};
    match current {
        Context => Track,
        Track => Off,
        Off => Context,
    }
}

fn top_tracks_time_range_param(
    time_range: rspotify::model::TimeRange,
) -> (&'static str, &'static str) {
    ("time_range", time_range.into())
}

fn paging_query<'a>(
    limit: &'a str,
    offset: &'a str,
    extra_params: &[(&'a str, &'a str)],
) -> Query<'a> {
    let mut params = Query::from([
        ("market", "from_token"),
        ("limit", limit),
        ("offset", offset),
    ]);
    for &(key, value) in extra_params {
        params.insert(key, value);
    }
    params
}

impl AppClient {
    /// Construct a new client
    pub async fn new() -> Result<Self> {
        let configs = config::get_config();
        let auth_config = AuthConfig::new(configs)?;

        let mut api_client = new_api_client()?;
        auth::prompt_for_user_token(&mut api_client, false)
            .await
            .context("authenticate Spotify Web API client")?;

        Ok(Self {
            spotify: Arc::new(spotify::Spotify::new()),
            http: reqwest::Client::builder()
                .timeout(HTTP_CLIENT_TIMEOUT)
                .build()
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        "Failed to build HTTP client with timeout, falling back to default: {err:#}"
                    );
                    reqwest::Client::new()
                }),
            auth_config,
            api_client,

            #[cfg(feature = "streaming")]
            stream_conn: Arc::new(Mutex::new(None)),
            session_reconnect: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    async fn token(&self) -> Result<String> {
        self.auto_reauth().await?;
        Ok(self
            .get_token()
            .lock()
            .await
            .unwrap()
            .as_ref()
            .context("no access token")?
            .access_token
            .clone())
    }

    /// Initialize the application's playback upon creating a new session or during startup.
    ///
    /// `resume` controls whether playback should be (re)started on the device we connect to.
    pub fn initialize_playback(&self, state: &SharedState, resume: bool) {
        tokio::task::spawn({
            let client = self.clone();
            let state = state.clone();
            async move {
                // Start the local desktop process before any startup sleeps or Web API
                // calls. Its MPRIS/Connect registration can then happen in parallel
                // with playback initialization.
                #[cfg(target_os = "linux")]
                let desktop_prelaunched = {
                    let desktop = config::get_config().app_config.desktop_spotify.clone();
                    match crate::desktop_spotify::launch_early_if_needed(&desktop) {
                        Ok(true) => {
                            state.push_success_toast(
                                "Starting Spotify desktop automatically while playback initializes…",
                            );
                            true
                        }
                        Ok(false) => false,
                        Err(err) => {
                            tracing::warn!("Failed to start Spotify desktop early: {err:#}");
                            state.push_error_toast(format!(
                                "Could not start Spotify desktop: {err:#}"
                            ));
                            false
                        }
                    }
                };

                // The main playback initialization logic is simple:
                // if there is no playback, connect to an available device
                //
                // However, because it takes time for Spotify server to show up new changes,
                // a retry logic is implemented to ensure the application's state is properly initialized
                let delay = std::time::Duration::from_secs(1);

                for attempt in 0u32..5 {
                    // The first attempt used to impose a full second of dead time.
                    // Retry delay is still useful after Spotify has had a chance to
                    // update server-side state.
                    if attempt > 0 {
                        tokio::time::sleep(delay).await;
                    }

                    match tokio::time::timeout(
                        PLAYBACK_FETCH_TIMEOUT,
                        client.retrieve_current_playback(&state, false),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            tracing::error!("Failed to retrieve current playback: {err:#}");
                            // Keep trying after rate-limit storms; give up only on hard failures.
                            if !is_rate_limit_msg(&err) {
                                return;
                            }
                            continue;
                        }
                        Err(_) => {
                            tracing::error!(
                                "Timed out after {PLAYBACK_FETCH_TIMEOUT:?} retrieving current playback during init"
                            );
                            continue;
                        }
                    }

                    // Wake the official desktop client when Connect cannot see the preferred
                    // device yet. Other speakers (e.g. Amazon "Everywhere") can still appear in
                    // the device list or own active playback while the desktop app is closed.
                    #[allow(unused_mut)]
                    let mut woke_desktop = false;
                    #[allow(unused_mut)]
                    let mut woke_for_preferred = false;
                    // Device the wake just made available; transfers must target it directly.
                    #[allow(unused_mut)]
                    let mut wake_target: Option<String> = None;
                    #[allow(unused_mut)]
                    let mut local_already_playing = false;
                    #[cfg(target_os = "linux")]
                    let mut minimize_after_transfer = false;
                    #[cfg(target_os = "linux")]
                    if attempt == 0 {
                        let desktop = config::get_config().app_config.desktop_spotify.clone();
                        if desktop.enable {
                            if crate::desktop_spotify::mpris_is_playing(&desktop.mpris_dest) {
                                local_already_playing = true;
                                tracing::info!(
                                    "Desktop Spotify is already playing locally (MPRIS); skipping wake/nudge"
                                );
                                state.push_success_toast("Spotify desktop is already playing");
                                // Connect often still omits the desktop client. Attach to it when
                                // listed; never fall through to another speaker (e.g. Everywhere).
                                woke_for_preferred = true;
                                wake_target = wait_for_preferred_device_with(
                                    &client,
                                    &state,
                                    ALREADY_PLAYING_DEVICE_WAIT,
                                    false,
                                )
                                .await;
                                woke_desktop = wake_target.is_some();
                                minimize_after_transfer =
                                    wake_target.is_some() && desktop.start_minimized;
                            } else {
                                // A process we just launched is necessarily the desktop
                                // endpoint we need; avoid waiting on another devices API call.
                                let should_wake = if desktop_prelaunched {
                                    true
                                } else {
                                    let (preferred_actively_playing, other_actively_playing) = {
                                        let preferred = config::get_config()
                                            .app_config
                                            .preferred_device
                                            .as_deref()
                                            .map(str::trim)
                                            .filter(|s| !s.is_empty());
                                        match state.player.read().playback.as_ref() {
                                            Some(p) if p.is_playing => {
                                                let on_preferred = preferred.is_some_and(|name| {
                                                    p.device.name.eq_ignore_ascii_case(name)
                                                });
                                                (on_preferred, !on_preferred)
                                            }
                                            _ => (false, false),
                                        }
                                    };
                                    match desktop_wake_needed(
                                        &client,
                                        preferred_actively_playing,
                                        other_actively_playing,
                                    )
                                    .await
                                    {
                                        Ok(needed) => needed,
                                        Err(err) => {
                                            tracing::warn!(
                                                "Failed to decide desktop Spotify wake: {err:#}; skipping wake"
                                            );
                                            false
                                        }
                                    }
                                };
                                if should_wake {
                                    let outcome =
                                        wake_desktop_spotify_if_enabled(&client, &state).await;
                                    woke_desktop = outcome.is_some();
                                    // Re-hide after Connect transfer even when we only nudged an
                                    // already-running tray instance (transfer can map the window).
                                    minimize_after_transfer =
                                        outcome.is_some() && desktop.start_minimized;
                                    if woke_desktop {
                                        woke_for_preferred = config::get_config()
                                            .app_config
                                            .preferred_device
                                            .as_deref()
                                            .is_some_and(|name| !name.trim().is_empty());
                                        if woke_for_preferred {
                                            wake_target =
                                                wait_for_preferred_device(&client, &state).await;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Existing playback normally needs no transfer. A successful desktop wake is
                    // the exception: select the newly visible preferred device even if another
                    // speaker owned playback when startup began.
                    if !should_select_device(state.player.read().playback.is_some(), woke_desktop) {
                        continue;
                    }

                    // Transfer onto the woken preferred device. Whether that
                    // transfer starts audio is `pause_after_nudge` (default: pause).
                    let keep_playing = keep_playing_after_desktop_wake(
                        wake_target.is_some(),
                        config::get_config()
                            .app_config
                            .desktop_spotify
                            .pause_after_nudge,
                        local_already_playing,
                    );

                    let device_ids = match device_ids_after_wake(wake_target, woke_for_preferred) {
                        DeviceIdsAfterWake::Preferred(ids) => ids,
                        DeviceIdsAfterWake::SkipTransfer => {
                            if local_already_playing {
                                tracing::info!(
                                    "Desktop Spotify is playing locally but preferred Connect device is not listed; leaving playback unchanged"
                                );
                            } else {
                                tracing::warn!(
                                    "Woke Spotify desktop but preferred device did not register; leaving playback unchanged"
                                );
                            }
                            continue;
                        }
                        DeviceIdsAfterWake::GenericFallback => {
                            match client.find_available_device_ids().await {
                                Ok(ids) => ids,
                                Err(err) => {
                                    tracing::error!("Failed to find an available device: {err:#}");
                                    Vec::new()
                                }
                            }
                        }
                    };

                    if device_ids.is_empty() {
                        tracing::warn!(
                            "No transferable Spotify Connect device found (attempt {})",
                            attempt + 1
                        );
                        continue;
                    }

                    let mut connected = false;
                    for id in device_ids {
                        tracing::info!(
                            "Trying to connect to device (id={id}, resume={resume}, keep_playing={keep_playing})"
                        );
                        match client.transfer_playback(&id, Some(keep_playing)).await {
                            Ok(()) => {
                                tracing::info!("Connection succeeded (device_id={id})!");
                                if resume {
                                    if let Err(err) =
                                        client.resume_playback(Some(id.as_ref()), None).await
                                    {
                                        tracing::warn!(
                                            "Failed to resume playback after reconnect: {err:#}"
                                        );
                                    }
                                }
                                // upon new connection, reset the buffered playback
                                state.player.write().buffered_playback = None;
                                client.update_playback(&state);
                                connected = true;

                                // Joining the Connect session raises the window Spotify
                                // was launched minimized into.
                                #[cfg(target_os = "linux")]
                                if minimize_after_transfer {
                                    crate::desktop_spotify::hide_window().await;
                                }
                                break;
                            }
                            Err(err) => {
                                tracing::warn!("Connection failed (device_id={id}): {err:#}");
                                // Try the next candidate (404s, offline devices, etc.).
                                if is_rate_limit_msg(&err) {
                                    sleep_rate_limit(attempt, None, "transfer playback").await;
                                }
                            }
                        }
                    }

                    if connected {
                        break;
                    }
                }
            }
        });
    }

    /// Create a new client session
    pub async fn new_session(&self, state: Option<&SharedState>, reauth: bool) -> Result<()> {
        // Capture whether playback was active *before* tearing down any existing streaming
        // connection. Shutting down the old `librespot` spirc pauses playback Spotify-side
        // (and a broken session leaves it paused too), so we use this to resume on the new
        // device rather than reconnecting in a paused state.
        let was_playing = state.is_some_and(|state| {
            state
                .player
                .read()
                .buffered_playback
                .as_ref()
                .is_some_and(|p| p.is_playing)
        });

        let session = self.auth_config.session();
        let creds = auth::get_creds(&self.auth_config, reauth, true).context("get credentials")?;
        self.spotify.set_session(session.clone()).await;

        #[allow(unused_mut)]
        let mut connected = false;

        #[cfg(feature = "streaming")]
        if let Some(state) = state {
            if state.is_streaming_enabled() {
                self.new_streaming_connection(state.clone(), session.clone(), creds.clone())
                    .await
                    .context("new streaming connection")?;
                connected = true;
            }
        }

        if !connected {
            // if session is not connected (triggered by `new_streaming_connection`), connect to the session
            session
                .connect(creds, true)
                .await
                .context("connect to a session")?;
        }

        tracing::info!("Used a new session for Spotify client.");

        if let Err(err) = self.refresh_token().await {
            tracing::warn!("Failed to refresh auth token after creating a new session: {err:#}");
        }

        if let Some(state) = state {
            // A fresh login starts with empty TTL caches. Reconnect (`g R`, invalid
            // session recovery) must keep them so the page watcher does not refetch
            // the current album and 429.
            if clear_memory_caches_on_new_session(reauth) {
                state.data.write().caches = MemoryCaches::new();
            }
            self.initialize_playback(state, was_playing);
        }

        Ok(())
    }

    /// Check if the current session is valid and if invalid, create a new session
    pub async fn check_valid_session(&self, state: &SharedState) -> Result<()> {
        if !self.spotify.session().await.is_invalid() {
            return Ok(());
        }

        // Serialize reconnects so a hung connect cannot pile up watchers / requests.
        let _guard = self.session_reconnect.lock().await;
        if !self.spotify.session().await.is_invalid() {
            return Ok(());
        }

        tracing::info!("Client's current session is invalid, creating a new session...");
        match tokio::time::timeout(
            SESSION_RECONNECT_TIMEOUT,
            self.new_session(Some(state), false),
        )
        .await
        {
            Ok(result) => result.context("create new client session")?,
            Err(_) => {
                anyhow::bail!(
                    "timed out after {SESSION_RECONNECT_TIMEOUT:?} recreating Spotify session"
                )
            }
        }
        Ok(())
    }

    /// Create a new streaming connection
    #[cfg(feature = "streaming")]
    pub async fn new_streaming_connection(
        &self,
        state: SharedState,
        session: librespot_core::Session,
        creds: librespot_core::authentication::Credentials,
    ) -> Result<()> {
        let new_conn =
            crate::streaming::new_connection(self.clone(), state, session, creds).await?;
        let mut stream_conn = self.stream_conn.lock();
        // shutdown old streaming connection and replace it with a new connection
        if let Some(conn) = stream_conn.as_ref() {
            if let Err(err) = conn.shutdown() {
                log::error!("Failed to shutdown old streaming connection: {err:#}");
            }
        }
        *stream_conn = Some(new_conn);
        Ok(())
    }

    /// Pause the integrated streaming client, if a connection exists.
    ///
    /// Returns `true` if a streaming connection was present and the pause
    /// command was issued. Used to suppress Spotify's auto-resume of the
    /// previous session on startup when `pause_on_startup` is enabled.
    #[cfg(feature = "streaming")]
    pub fn pause_streaming_on_startup(&self) -> bool {
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => {
                if let Err(err) = spirc.pause() {
                    tracing::warn!("Failed to pause integrated client on startup: {err:#}");
                }
                true
            }
            None => false,
        }
    }

    /// Handle a player request, return a new playback metadata on success
    pub async fn handle_player_request(
        &self,
        request: PlayerRequest,
        mut playback: Option<PlaybackMetadata>,
    ) -> Result<Option<PlaybackMetadata>> {
        // handle requests that don't require an active playback
        match request {
            PlayerRequest::TransferPlayback(device_id, force_play) => {
                // `TransferPlayback` needs to be handled separately from other player requests
                // because `TransferPlayback` doesn't require an active playback
                self.transfer_playback(&device_id, Some(force_play)).await?;
                tracing::info!("Transferred playback to device with id={}", device_id);
                return Ok(None);
            }
            PlayerRequest::StartPlayback(p, shuffle) => {
                // Set the playback's shuffle state if specified in the request
                if let (Some(shuffle), Some(playback)) = (shuffle, playback.as_mut()) {
                    playback.shuffle_state = shuffle;
                }
                let device_id = playback.as_ref().and_then(|p| p.device_id.as_deref());
                self.start_playback(p, device_id).await?;
                // For some reasons, when starting a new playback, the integrated `spotify_player`
                // client doesn't respect the initial shuffle state, so we need to manually update the state
                if let Some(ref playback) = playback {
                    self.shuffle(playback.shuffle_state, device_id).await?;
                }
                return Ok(None);
            }
            _ => {}
        }

        let mut playback = playback.context("no playback found")?;
        let device_id = playback.device_id.as_deref();

        match request {
            PlayerRequest::NextTrack => self.next_track(device_id).await?,
            PlayerRequest::PreviousTrack => self.previous_track(device_id).await?,
            PlayerRequest::Resume => {
                if !playback.is_playing {
                    self.resume_playback(device_id, None).await?;
                    playback.is_playing = true;
                }
            }

            PlayerRequest::Pause => {
                if playback.is_playing {
                    self.pause_playback(device_id).await?;
                    playback.is_playing = false;
                }
            }
            PlayerRequest::ResumePause => {
                if playback.is_playing {
                    self.pause_playback(device_id).await?;
                } else {
                    self.resume_playback(device_id, None).await?;
                }
                playback.is_playing = !playback.is_playing;
            }
            PlayerRequest::SeekTrack(position_ms) => {
                self.seek_track(position_ms, device_id).await?;
            }
            PlayerRequest::Repeat => {
                let next_repeat_state = next_repeat_state(playback.repeat_state);

                self.repeat(next_repeat_state, device_id).await?;

                playback.repeat_state = next_repeat_state;
            }
            PlayerRequest::Shuffle => {
                self.shuffle(!playback.shuffle_state, device_id).await?;

                playback.shuffle_state = !playback.shuffle_state;
            }
            PlayerRequest::Volume(volume) => {
                self.volume(volume, device_id).await?;

                playback.volume = Some(u32::from(volume));
                playback.mute_state = None;
            }
            PlayerRequest::ToggleMute => {
                let new_mute_state = match playback.mute_state {
                    None => {
                        self.volume(0, device_id).await?;
                        Some(playback.volume.unwrap_or_default())
                    }
                    Some(volume) => {
                        self.volume(volume as u8, device_id).await?;
                        None
                    }
                };

                playback.mute_state = new_mute_state;
            }
            PlayerRequest::StartPlayback(..) => {
                anyhow::bail!("`StartPlayback` should be handled earlier")
            }
            PlayerRequest::TransferPlayback(..) => {
                anyhow::bail!("`TransferPlayback` should be handled earlier")
            }
        }

        Ok(Some(playback))
    }

    /// Handle a client request
    pub(crate) async fn handle_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
    ) -> Result<()> {
        let timer = tokio::time::Instant::now();

        match request {
            ClientRequest::GetBrowseCategories => {
                let categories = self.browse_categories().await?;
                state.data.write().browse.categories = categories;
            }
            ClientRequest::GetBrowseCategoryPlaylists(category) => {
                let playlists = self.browse_category_playlists(&category.id).await?;
                state
                    .data
                    .write()
                    .browse
                    .category_playlists
                    .insert(category.id, playlists);
            }
            ClientRequest::GetLyrics { track_id } => {
                let uri = track_id.uri();
                if !state.data.read().caches.lyrics.contains_key(&uri) {
                    let lyrics = self.lyrics(track_id).await?;
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(uri, lyrics, *TTL_CACHE_DURATION);
                }
            }
            #[cfg(feature = "streaming")]
            ClientRequest::RestartIntegratedClient => {
                let _guard = self.session_reconnect.lock().await;
                match tokio::time::timeout(
                    SESSION_RECONNECT_TIMEOUT,
                    self.new_session(Some(state), false),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        anyhow::bail!(
                            "timed out after {SESSION_RECONNECT_TIMEOUT:?} restarting integrated client"
                        )
                    }
                }
            }
            ClientRequest::GetCurrentUser => {
                let user = self.current_user().await?;
                state.data.write().user_data.user = Some(user);
            }
            ClientRequest::Player(request) => {
                let playback = state.player.read().buffered_playback.clone();
                let playback = self.handle_player_request(request, playback).await?;
                state.player.write().buffered_playback = playback;
                self.update_playback(state);
            }
            ClientRequest::GetCurrentPlayback => {
                self.retrieve_current_playback(state, true).await?;
            }
            ClientRequest::GetDevices => {
                #[allow(unused_mut)]
                let mut devices: Vec<Device> = self
                    .available_devices()
                    .await?
                    .into_iter()
                    .filter_map(Device::try_from_device)
                    .collect();

                #[cfg(feature = "streaming")]
                if self.is_integrated_streaming_active() {
                    self.ensure_integrated_device(&mut devices).await;
                }

                state.player.write().devices = devices;
            }
            ClientRequest::GetUserPlaylists => {
                let playlists = self.current_user_playlists().await?;
                let node = state.data.read().user_data.playlist_folder_node.clone();
                let playlists = if let Some(node) = node.filter(|n| !n.children.is_empty()) {
                    crate::playlist_folders::structurize(playlists, &node.children)
                } else {
                    playlists
                        .into_iter()
                        .map(PlaylistFolderItem::Playlist)
                        .collect()
                };
                store_data_into_file_cache(
                    FileCacheKey::Playlists,
                    &config::get_config().cache_folder,
                    &playlists,
                )
                .context("store user's playlists into the cache folder")?;
                state.data.write().user_data.playlists = playlists;
            }
            ClientRequest::GetUserFollowedArtists => {
                let artists = self.current_user_followed_artists().await?;
                store_data_into_file_cache(
                    FileCacheKey::FollowedArtists,
                    &config::get_config().cache_folder,
                    &artists,
                )
                .context("store user's followed artists into the cache folder")?;
                state.data.write().user_data.followed_artists = artists;
            }
            ClientRequest::GetUserSavedAlbums => {
                let albums = self.current_user_saved_albums().await?;
                store_data_into_file_cache(
                    FileCacheKey::SavedAlbums,
                    &config::get_config().cache_folder,
                    &albums,
                )
                .context("store user's saved albums into the cache folder")?;
                state.data.write().user_data.saved_albums = albums;
            }
            ClientRequest::GetUserSavedShows => {
                let shows = self.current_user_saved_shows().await?;
                store_data_into_file_cache(
                    FileCacheKey::SavedShows,
                    &config::get_config().cache_folder,
                    &shows,
                )
                .context("store user's saved shows into the cache folder")?;
                state.data.write().user_data.saved_shows = shows;
            }
            ClientRequest::GetContext(context) => {
                let uri = context.uri();
                // Liked tracks must always be refreshed to keep user_data.saved_tracks in sync.
                let cache_miss = uri != USER_LIKED_TRACKS_URI
                    && !state.data.read().caches.context.contains_key(&uri);
                let is_liked = uri == USER_LIKED_TRACKS_URI;
                if cache_miss || is_liked {
                    let ctx = match context {
                        ContextId::Playlist(playlist_id) => {
                            self.playlist_context(playlist_id).await?
                        }
                        ContextId::Album(album_id) => self.album_context(album_id).await?,
                        ContextId::Artist(artist_id) => self.artist_context(artist_id).await?,
                        ContextId::Tracks(tracks_id) => match tracks_id.uri.as_str() {
                            USER_TOP_TRACKS_URI => {
                                self.user_top_tracks_context(
                                    rspotify::model::TimeRange::MediumTerm,
                                    "User's top tracks (~6 months)",
                                )
                                .await?
                            }
                            USER_TOP_TRACKS_SHORT_TERM_URI => {
                                self.user_top_tracks_context(
                                    rspotify::model::TimeRange::ShortTerm,
                                    "User's top tracks (~4 weeks)",
                                )
                                .await?
                            }
                            USER_TOP_TRACKS_LONG_TERM_URI => {
                                self.user_top_tracks_context(
                                    rspotify::model::TimeRange::LongTerm,
                                    "User's top tracks (~1 year)",
                                )
                                .await?
                            }
                            USER_RECENTLY_PLAYED_TRACKS_URI => Context::Tracks {
                                tracks: self.current_user_recently_played_tracks().await?,
                                desc: "User's recently played tracks".to_string(),
                            },
                            USER_LIKED_TRACKS_URI => {
                                let tracks = self.current_user_saved_tracks().await?;
                                let tracks_hm = tracks
                                    .iter()
                                    .map(|t| (t.id.uri(), t.clone()))
                                    .collect::<HashMap<_, _>>();
                                store_data_into_file_cache(
                                    FileCacheKey::SavedTracks,
                                    &config::get_config().cache_folder,
                                    &tracks_hm,
                                )
                                .context("store user's saved tracks into the cache folder")?;
                                state.data.write().user_data.saved_tracks = tracks_hm;
                                Context::Tracks {
                                    tracks,
                                    desc: "User's liked tracks".to_string(),
                                }
                            }
                            u if u.starts_with("radio:") => Context::Tracks {
                                tracks: self.radio_tracks(u["radio:".len()..].to_string()).await?,
                                desc: tracks_id.kind.clone(),
                            },
                            uri => anyhow::bail!("unsupported Tracks context: {uri}"),
                        },
                        ContextId::Show(show_id) => self.show_context(show_id).await?,
                    };

                    state
                        .data
                        .write()
                        .caches
                        .context
                        .insert(uri, ctx, *TTL_CACHE_DURATION);
                }
            }
            ClientRequest::Search(query) => {
                if !state.data.read().caches.search.contains_key(&query) {
                    let results = self.search(&query).await?;

                    state
                        .data
                        .write()
                        .caches
                        .search
                        .insert(query, results, *TTL_CACHE_DURATION);
                }
            }

            ClientRequest::AddPlayableToQueue(playable_id) => {
                self.add_item_to_queue(playable_id, None).await?;
            }
            ClientRequest::AddPlayableToPlaylist(playlist_id, playable_id) => {
                self.add_item_to_playlist(state, playlist_id, playable_id)
                    .await?;
            }
            ClientRequest::AddAlbumToQueue(album_id) => {
                let album_context = self.album_context(album_id).await?;

                if let Context::Album { album: _, tracks } = album_context {
                    for track in tracks {
                        self.add_item_to_queue(PlayableId::Track(track.id), None)
                            .await?;
                    }
                }
            }
            ClientRequest::DeleteTrackFromPlaylist(playlist_id, track_id) => {
                self.delete_track_from_playlist(state, playlist_id, track_id)
                    .await?;
            }
            ClientRequest::AddToLibrary(item) => {
                self.add_to_library(state, item).await?;
            }
            ClientRequest::DeleteFromLibrary(id) => {
                self.delete_from_library(state, id).await?;
            }
            ClientRequest::GetCurrentUserQueue => {
                let queue = self.current_user_queue().await?;
                state.player.write().queue = Some(queue);
            }
            ClientRequest::ReorderPlaylistItems {
                playlist_id,
                insert_index,
                range_start,
                range_length,
                snapshot_id,
            } => {
                self.reorder_playlist_items(
                    state,
                    playlist_id,
                    insert_index,
                    range_start,
                    range_length,
                    snapshot_id.as_deref(),
                )
                .await?;
            }
            ClientRequest::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            } => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone())
                    .unwrap();
                self.create_new_playlist(
                    state,
                    user_id,
                    playlist_name.as_str(),
                    public,
                    collab,
                    desc.as_str(),
                )
                .await?;
            }
        }

        tracing::info!(
            "Successfully handled the client request, took: {}ms",
            timer.elapsed().as_millis()
        );

        Ok(())
    }

    /// Get lyrics of a given track, return None if no lyrics is available
    pub async fn lyrics(&self, track_id: TrackId<'static>) -> Result<Option<Lyrics>> {
        let session = self.spotify.session().await;
        let uri = SpotifyUri::from_uri(&track_id.uri())?;
        match uri {
            SpotifyUri::Track { id } => {
                match librespot_metadata::Lyrics::get(&session, &id).await {
                    Ok(lyrics) => Ok(Some(lyrics.into())),
                    Err(err) => {
                        if err.to_string().to_lowercase().contains("not found") {
                            Ok(None)
                        } else {
                            Err(err.into())
                        }
                    }
                }
            }
            _ => Ok(None),
        }
    }

    /// Get user available devices
    pub async fn available_devices(&self) -> Result<Vec<rspotify::model::Device>> {
        Ok(self.device().await?)
    }

    pub fn update_playback(&self, state: &SharedState) {
        // After handling a request changing the player's playback,
        // update the playback state by making a few get-playback requests.
        //
        // Q: Why do we need more than one request to update the playback?
        // A: It might take a while for Spotify server to reflect the new change,
        // making additional requests can help ensure that the playback state is always up-to-date.
        let client = self.clone();
        let state = state.clone();
        tokio::task::spawn(async move {
            let delay = std::time::Duration::from_secs(1);
            for attempt in 0u32..3 {
                tokio::time::sleep(delay).await;
                match tokio::time::timeout(
                    PLAYBACK_FETCH_TIMEOUT,
                    client.retrieve_current_playback(&state, false),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        tracing::error!(
                            "Encountered an error when updating the playback state: {err:#}"
                        );
                        if is_rate_limit_msg(&err) {
                            sleep_rate_limit(attempt, None, "update playback").await;
                        }
                    }
                    Err(_) => {
                        tracing::error!(
                            "Timed out after {PLAYBACK_FETCH_TIMEOUT:?} updating the playback state"
                        );
                    }
                }
            }
        });
    }

    /// Get Spotify's available browse categories
    pub async fn browse_categories(&self) -> Result<Vec<Category>> {
        let first_page = self
            .categories_manual(Some("EN"), None, Some(50), None)
            .await?;

        Ok(first_page.items.into_iter().map(Category::from).collect())
    }

    /// Get Spotify's available browse playlists of a given category
    pub async fn browse_category_playlists(&self, category_id: &str) -> Result<Vec<Playlist>> {
        // TODO: this should use `rspotify::category_playlists_manual` API instead of `http_get`
        // The current implementation is a workaround for https://github.com/ramsayleung/rspotify/issues/535

        // Ok(self
        //     .category_playlists_manual(
        //         category_id,
        //         Some(rspotify::model::Market::FromToken),
        //         Some(50),
        //         None,
        //     )
        //     .await?
        //     .items
        //     .into_iter()
        //     .map(Into::into)
        //     .collect())

        #[derive(Deserialize, Debug)]
        struct BrowseCategoryPlaylistsResponse {
            playlists: rspotify::model::Page<serde_json::Value>,
        }

        Ok(self
            .http_get::<BrowseCategoryPlaylistsResponse>(
                &format!("{SPOTIFY_API_ENDPOINT}/browse/categories/{category_id}/playlists"),
                &Query::from([("limit", "50")]),
            )
            .await?
            .playlists
            .items
            .into_iter()
            .filter_map(|item| {
                serde_json::from_value::<rspotify::model::SimplifiedPlaylist>(item).ok()
            })
            .map(Into::into)
            .collect())
    }

    /// Find available Connect devices to transfer playback to, ordered by preference.
    ///
    /// Preference order:
    /// 1. Currently active device (if any) — returned alone
    /// 2. `preferred_device` name match (config)
    /// 3. Real API devices (not the synthetic integrated librespot device)
    /// 4. Integrated streaming device — only when this process is actually streaming
    async fn find_available_device_ids(&self) -> Result<Vec<String>> {
        let api_devices = self.available_devices().await?;

        // if there is an active device, return it alone
        if let Some(d) = api_devices.iter().find(|d| d.is_active) {
            if let Some(id) = d.id.clone() {
                return Ok(vec![id]);
            }
        }

        #[allow(unused_mut)]
        // mutated only when the streaming feature injects the integrated device
        let mut devices = api_devices
            .into_iter()
            .filter_map(Device::try_from_device)
            .collect::<Vec<_>>();

        #[cfg(feature = "streaming")]
        let include_integrated = self.is_integrated_streaming_active();
        #[cfg(not(feature = "streaming"))]
        let include_integrated = false;

        #[cfg(feature = "streaming")]
        if include_integrated {
            self.ensure_integrated_device(&mut devices).await;
        }

        tracing::info!(
            "no active device found, available devices: {devices:?} (include_integrated={include_integrated})"
        );

        let preferred = config::get_config().app_config.preferred_device.as_deref();

        Ok(order_transfer_device_ids(
            &devices,
            preferred,
            include_integrated,
        ))
    }

    /// Whether this process currently has (or will have) a real integrated
    /// librespot Connect endpoint. When `enable_streaming = Never`, the session
    /// still has a `device_id`, but transferring to it returns HTTP 404.
    #[cfg(feature = "streaming")]
    fn is_integrated_streaming_active(&self) -> bool {
        self.stream_conn.lock().is_some()
    }

    /// Ensures the integrated librespot device (of *this* running instance) is present in `devices`.
    ///
    /// The integrated device may not show up in the device list returned by the Spotify API because
    /// 1. The device is just initialized and hasn't been registered in Spotify server.
    ///    Related issue/discussion: <https://github.com/aome510/spotify-player/issues/79>
    /// 2. The device list is empty. This might be because user doesn't specify their own client ID.
    ///    By default, the application uses Spotify web app's client ID, which doesn't have
    ///    access to user's active devices.
    #[cfg(feature = "streaming")]
    async fn ensure_integrated_device(&self, devices: &mut Vec<Device>) {
        let session = self.spotify.session().await;
        let session_device_id = session.device_id().to_string();

        // Mark the integrated device if it's already in the list; otherwise, add it, so it's
        // always present without duplicating an entry the API already returned.
        match devices.iter_mut().find(|d| d.id == session_device_id) {
            Some(device) => device.is_integrated = true,
            None => devices.insert(
                0,
                Device {
                    id: session_device_id,
                    name: config::get_config().app_config.device.name.clone(),
                    is_integrated: true,
                },
            ),
        }
    }

    /// Get the saved (liked) tracks of the current user
    pub async fn current_user_saved_tracks(&self) -> Result<Vec<Track>> {
        let tracks = self
            .all_paging_items::<rspotify::model::SavedTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/tracks"),
                0, // we don't know the total number of saved tracks beforehand
            )
            .await?;

        Ok(tracks
            .into_iter()
            .filter_map(|t| Track::try_from_full_track(t.track))
            .collect())
    }

    /// Get the recently played tracks of the current user
    pub async fn current_user_recently_played_tracks(&self) -> Result<Vec<Track>> {
        let first_page = self.current_user_recently_played(Some(50), None).await?;

        let play_histories = self.all_cursor_based_paging_items(first_page).await?;

        // de-duplicate the tracks returned from the recently-played API
        let mut tracks = Vec::<Track>::new();
        for history in play_histories {
            if !tracks.iter().any(|t| t.name == history.track.name) {
                if let Some(track) = Track::try_from_full_track(history.track) {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }

    /// Get the top tracks of the current user over a Spotify affinity time range.
    pub async fn current_user_top_tracks(
        &self,
        time_range: rspotify::model::TimeRange,
    ) -> Result<Vec<Track>> {
        let tracks = self
            .all_paging_items_with::<rspotify::model::FullTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/top/tracks"),
                0, // we don't know the total number of top tracks beforehand
                &[top_tracks_time_range_param(time_range)],
            )
            .await?;

        Ok(tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect())
    }

    async fn user_top_tracks_context(
        &self,
        time_range: rspotify::model::TimeRange,
        desc: &str,
    ) -> Result<Context> {
        Ok(Context::Tracks {
            tracks: self.current_user_top_tracks(time_range).await?,
            desc: desc.to_string(),
        })
    }

    /// Get all playlists of the current user
    pub async fn current_user_playlists(&self) -> Result<Vec<Playlist>> {
        let playlists = self
            .all_paging_items::<rspotify::model::SimplifiedPlaylist>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/playlists"),
                0, // we don't know the total number of playlists beforehand
            )
            .await?;

        Ok(playlists
            .into_iter()
            .map(std::convert::Into::into)
            .collect())
    }

    /// Get all followed artists of the current user
    pub async fn current_user_followed_artists(&self) -> Result<Vec<Artist>> {
        let first_page = self
            .deref()
            .current_user_followed_artists(None, None)
            .await?;

        // followed artists pagination is handled different from
        // other paginations. The endpoint uses cursor-based pagination.
        let mut artists = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self
                .http_get::<rspotify::model::CursorPageFullArtists>(&url, &Query::new())
                .await?
                .artists;
            artists.append(&mut next_page.items);
            maybe_next = next_page.next;
        }

        // converts `rspotify::model::FullArtist` into `state::Artist`
        Ok(artists.into_iter().map(std::convert::Into::into).collect())
    }

    /// Get all saved albums of the current user
    pub async fn current_user_saved_albums(&self) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items::<rspotify::model::SavedAlbum>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/albums"),
                0, // we don't know the total number of saved albums beforehand
            )
            .await?;

        // Converts `rspotify::model::SavedAlbum` into `state::Album`
        Ok(albums.into_iter().map(Album::from).collect())
    }

    /// Get all saved shows of the current user
    pub async fn current_user_saved_shows(&self) -> Result<Vec<Show>> {
        let shows = self
            .all_paging_items::<rspotify::model::Show>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/shows"),
                0, // we don't know the total number of saved shows beforehand
            )
            .await?;

        Ok(shows.into_iter().map(|s| s.show.into()).collect())
    }

    /// Get all albums of an artist
    pub async fn artist_albums(&self, artist_id: ArtistId<'_>) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items::<rspotify::model::SimplifiedAlbum>(
                &format!(
                    "{SPOTIFY_API_ENDPOINT}/artists/{}/albums?include_groups=album,single",
                    artist_id.id()
                ),
                0, // we don't know the total number of artist albums beforehand
            )
            .await?
            .into_iter()
            .filter_map(Album::try_from_simplified_album)
            .collect();

        Ok(AppClient::process_artist_albums(albums))
    }

    /// Start a playback
    async fn start_playback(&self, playback: Playback, device_id: Option<&str>) -> Result<()> {
        match playback {
            Playback::Context(id, offset) => match id {
                ContextId::Album(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Artist(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Playlist(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Show(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Tracks(_) => {
                    anyhow::bail!("`StartPlayback` request for `tracks` context is not supported")
                }
            },
            Playback::URIs(ids, offset) => {
                self.start_uris_playback(ids, device_id, offset, None)
                    .await?;
            }
        }

        Ok(())
    }

    /// Get recommendation (radio) tracks based on a seed
    pub async fn radio_tracks(&self, seed_uri: String) -> Result<Vec<Track>> {
        #[derive(Debug, Deserialize)]
        struct TrackData {
            original_gid: String,
        }
        #[derive(Debug, Deserialize)]
        struct RadioStationResponse {
            tracks: Vec<TrackData>,
        }

        let session = self.spotify.session().await;

        // Get an autoplay URI from the seed URI.
        // The return URI is a Spotify station's URI
        let autoplay_query_url = format!("hm://autoplay-enabled/query?uri={seed_uri}");
        let response = session
            .mercury()
            .get(autoplay_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get autoplay URI: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get autoplay URI: got non-OK status code: {}",
                response.status_code
            );
        }
        let autoplay_uri = String::from_utf8(response.payload[0].clone())?;

        // Retrieve radio's data based on the autoplay URI
        let radio_query_url = format!("hm://radio-apollo/v3/stations/{autoplay_uri}");
        let response = session
            .mercury()
            .get(radio_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get radio data of {autoplay_uri}: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get radio data of {autoplay_uri}: got non-OK status code: {}",
                response.status_code
            );
        }

        // Parse a list consisting of IDs of tracks inside the radio station
        let track_ids = serde_json::from_slice::<RadioStationResponse>(&response.payload[0])?
            .tracks
            .into_iter()
            .filter_map(|t| TrackId::from_id(t.original_gid).ok());

        // Retrieve tracks based on IDs
        let tracks = self
            .tracks(track_ids, Some(rspotify::model::Market::FromToken))
            .await?;
        let mut tracks: Vec<_> = tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect();

        // Track-seeded radios in the official Spotify clients include the seed track itself
        // as the first item in the generated session.
        if let Ok(track_id) = TrackId::from_uri(&seed_uri) {
            match self.track(track_id).await {
                Ok(track) => move_seed_track_to_front(&mut tracks, track),
                Err(err) => {
                    tracing::warn!("Failed to fetch track radio seed {seed_uri}: {err:#}");
                }
            }
        }

        Ok(tracks)
    }

    /// Search for items (tracks, artists, albums, playlists) matching a given query
    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        let (
            track_result,
            artist_result,
            album_result,
            playlist_result,
            show_result,
            episode_result,
        ) = tokio::try_join!(
            self.search_specific_type(query, rspotify::model::SearchType::Track),
            self.search_specific_type(query, rspotify::model::SearchType::Artist),
            self.search_specific_type(query, rspotify::model::SearchType::Album),
            self.search_specific_type(query, rspotify::model::SearchType::Playlist),
            self.search_specific_type(query, rspotify::model::SearchType::Show),
            self.search_specific_type(query, rspotify::model::SearchType::Episode)
        )?;

        let (tracks, artists, albums, playlists, shows, episodes) = (
            match track_result {
                rspotify::model::SearchResult::Tracks(p) => p
                    .items
                    .into_iter()
                    .filter_map(Track::try_from_full_track)
                    .collect(),
                _ => anyhow::bail!("expect a track search result"),
            },
            match artist_result {
                rspotify::model::SearchResult::Artists(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect an artist search result"),
            },
            match album_result {
                rspotify::model::SearchResult::Albums(p) => p
                    .items
                    .into_iter()
                    .filter_map(Album::try_from_simplified_album)
                    .collect(),
                _ => anyhow::bail!("expect an album search result"),
            },
            match playlist_result {
                rspotify::model::SearchResult::Playlists(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect a playlist search result"),
            },
            match show_result {
                rspotify::model::SearchResult::Shows(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect a show search result"),
            },
            match episode_result {
                rspotify::model::SearchResult::Episodes(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect a episode search result"),
            },
        );

        Ok(SearchResults {
            tracks,
            artists,
            albums,
            playlists,
            shows,
            episodes,
        })
    }

    /// Search for items of a specific type matching a given query
    pub async fn search_specific_type(
        &self,
        query: &str,
        typ: rspotify::model::SearchType,
    ) -> Result<rspotify::model::SearchResult> {
        Ok(self
            .deref()
            .search(query, typ, None, None, None, None)
            .await?)
    }

    /// Add a playable item to a playlist
    pub async fn add_item_to_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        playable_id: PlayableId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.playlist_remove_all_occurrences_of_items(
            playlist_id.as_ref(),
            [playable_id.as_ref()],
            None,
        )
        .await?;

        self.playlist_add_items(playlist_id.as_ref(), [playable_id.as_ref()], None)
            .await?;

        // After adding a new track to a playlist, remove the cache of that playlist to force refetching new data
        state.data.write().caches.context.remove(&playlist_id.uri());

        Ok(())
    }

    /// Remove a track from a playlist
    pub async fn delete_track_from_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        track_id: TrackId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.playlist_remove_all_occurrences_of_items(
            playlist_id.as_ref(),
            [PlayableId::Track(track_id.as_ref())],
            None,
        )
        .await?;

        // After making a delete request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            tracks.retain(|t| t.id != track_id);
        }

        Ok(())
    }

    /// Reorder items in a playlist
    async fn reorder_playlist_items(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        insert_index: usize,
        range_start: usize,
        range_length: Option<usize>,
        snapshot_id: Option<&str>,
    ) -> Result<()> {
        let insert_before = if insert_index > range_start {
            insert_index + 1
        } else {
            insert_index
        };

        self.playlist_reorder_items(
            playlist_id.clone(),
            Some(range_start as i32),
            Some(insert_before as i32),
            range_length.map(|range_length| range_length as u32),
            snapshot_id,
        )
        .await?;

        // After making a reorder request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            let track = tracks.remove(range_start);
            tracks.insert(insert_index, track);
        }

        Ok(())
    }

    /// Add a Spotify item to current user's library.
    async fn add_to_library(&self, state: &SharedState, item: Item) -> Result<()> {
        // Before adding new item, checks if that item already exists in the library to avoid adding a duplicated item.
        match item {
            Item::Track(track) => {
                let contains = self
                    .current_user_saved_tracks_contains([track.id.as_ref()])
                    .await?;
                if !contains[0] {
                    self.current_user_saved_tracks_add([track.id.as_ref()])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .saved_tracks
                        .insert(track.id.uri(), track);
                }
            }
            Item::Album(album) => {
                let contains = self
                    .current_user_saved_albums_contains([album.id.as_ref()])
                    .await?;
                if !contains[0] {
                    self.current_user_saved_albums_add([album.id.as_ref()])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_albums.insert(0, album);
                }
            }
            Item::Artist(artist) => {
                let follows = self.user_artist_check_follow([artist.id.as_ref()]).await?;
                if !follows[0] {
                    self.user_follow_artists([artist.id.as_ref()]).await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .followed_artists
                        .insert(0, artist);
                }
            }
            Item::Playlist(playlist) => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone());

                if let Some(user_id) = user_id {
                    let follows = self
                        .playlist_check_follow(playlist.id.as_ref(), &[user_id])
                        .await?;
                    if !follows[0] {
                        self.playlist_follow(playlist.id.as_ref(), None).await?;
                        // update the in-memory `user_data`
                        state
                            .data
                            .write()
                            .user_data
                            .playlists
                            .insert(0, PlaylistFolderItem::Playlist(playlist));
                    }
                }
            }
            Item::Show(show) => {
                let follows = self.check_users_saved_shows([show.id.as_ref()]).await?;
                if !follows[0] {
                    self.save_shows([show.id.as_ref()]).await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_shows.insert(0, show);
                }
            }
        }
        Ok(())
    }

    // Delete a Spotify item from user's library
    async fn delete_from_library(&self, state: &SharedState, id: ItemId) -> Result<()> {
        match id {
            ItemId::Track(id) => {
                let uri = id.uri();
                self.current_user_saved_tracks_delete([id]).await?;
                state.data.write().user_data.saved_tracks.remove(&uri);
            }
            ItemId::Album(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_albums
                    .retain(|a| a.id != id);
                self.current_user_saved_albums_delete([id]).await?;
            }
            ItemId::Artist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .followed_artists
                    .retain(|a| a.id != id);
                self.user_unfollow_artists([id]).await?;
            }
            ItemId::Playlist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .playlists
                    .retain(|item| match item {
                        PlaylistFolderItem::Playlist(p) => p.id != id,
                        PlaylistFolderItem::Folder(_) => true,
                    });
                self.playlist_unfollow(id).await?;
            }
            ItemId::Show(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_shows
                    .retain(|s| s.id != id);
                self.remove_users_saved_shows([id], Some(rspotify::model::Market::FromToken))
                    .await?;
            }
        }
        Ok(())
    }

    /// Get a track data
    pub async fn track(&self, track_id: TrackId<'_>) -> Result<Track> {
        Track::try_from_full_track(
            self.deref()
                .track(track_id, Some(rspotify::model::Market::FromToken))
                .await?,
        )
        .context("convert FullTrack into Track")
    }

    /// Get a playlist context data
    pub async fn playlist_context(&self, playlist_id: PlaylistId<'_>) -> Result<Context> {
        let playlist_uri = playlist_id.uri();
        tracing::info!("Get playlist context: {}", playlist_uri);

        let playlist = self
            .playlist(
                playlist_id.clone(),
                None,
                Some(rspotify::model::Market::FromToken),
            )
            .await?;

        let tracks = self
            .all_paging_items(
                &format!(
                    "{SPOTIFY_API_ENDPOINT}/playlists/{}/tracks",
                    playlist_id.id(),
                ),
                playlist.tracks.total as usize,
            )
            .await?
            .into_iter()
            .filter_map(Track::try_from_playlist_item)
            .collect::<Vec<_>>();

        Ok(Context::Playlist {
            playlist: playlist.into(),
            tracks,
        })
    }

    /// Get an album context data
    pub async fn album_context(&self, album_id: AlbumId<'_>) -> Result<Context> {
        let album_uri = album_id.uri();
        tracing::info!("Get album context: {}", album_uri);

        let album = self
            .album(album_id.clone(), Some(rspotify::model::Market::FromToken))
            .await?;

        let total_tracks = album.tracks.total as usize;

        // converts `rspotify::model::FullAlbum` into `state::Album`
        let album: Album = album.into();

        // get the album's tracks
        let tracks = self
            .all_paging_items(
                &format!("{SPOTIFY_API_ENDPOINT}/albums/{}/tracks", album_id.id()),
                total_tracks,
            )
            .await?
            .into_iter()
            .filter_map(|t| {
                // simplified track doesn't have album so
                // we need to manually include one during
                // converting into `state::Track`
                Track::try_from_simplified_track(t).map(|mut t| {
                    t.album = Some(album.clone());
                    t
                })
            })
            .collect::<Vec<_>>();

        Ok(Context::Album { album, tracks })
    }

    /// Get an artist context data
    pub async fn artist_context(&self, artist_id: ArtistId<'_>) -> Result<Context> {
        let artist_uri = artist_id.uri();
        tracing::info!("Get artist context: {}", artist_uri);

        // get the artist's information, including top tracks, related artists, and albums

        let artist = self
            .artist(artist_id.as_ref())
            .await
            .context("get artist")?
            .into();

        let top_tracks = self
            .artist_top_tracks(artist_id.as_ref(), Some(rspotify::model::Market::FromToken))
            .await
            .context("get artist's top tracks")?
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect::<Vec<_>>();

        #[allow(deprecated)]
        let related_artists = self
            .artist_related_artists(artist_id.as_ref())
            .await
            .ok()
            .unwrap_or_default()
            .into_iter()
            .map(std::convert::Into::into)
            .collect::<Vec<_>>();

        let albums = self
            .artist_albums(artist_id.as_ref())
            .await
            .context("get artist's albums")?;

        Ok(Context::Artist {
            artist,
            top_tracks,
            albums,
            related_artists,
        })
    }

    /// Get a show context data
    pub async fn show_context(&self, show_id: ShowId<'_>) -> Result<Context> {
        let show_uri = show_id.uri();
        tracing::info!("Get show context: {}", show_uri);

        let show = self.get_a_show(show_id.clone(), None).await?;

        // get the show's episodes
        let episodes = self
            .all_paging_items::<rspotify::model::SimplifiedEpisode>(
                &format!("{SPOTIFY_API_ENDPOINT}/shows/{}/episodes", show_id.id()),
                show.episodes.total as usize,
            )
            .await?
            .into_iter()
            .map(std::convert::Into::into)
            .collect::<Vec<_>>();

        // converts `rspotify::model::FullShow` into `state::Show`
        let show: Show = show.into();

        Ok(Context::Show { show, episodes })
    }

    /// Make a GET HTTP request to the Spotify server
    async fn http_get<T>(&self, url: &str, payload: &Query<'_>) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut attempt = 0u32;
        loop {
            let access_token = self.token().await.context("get token")?;
            tracing::debug!("{access_token} {url}");

            let response = self
                .http
                .get(url)
                .query(payload)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {access_token}"),
                )
                .send()
                .await?;

            let status = response.status();
            let retry_after = parse_retry_after_secs(response.headers());
            let text = process_spotify_api_response(&response.text().await?);
            tracing::debug!("{text}");

            if status == StatusCode::TOO_MANY_REQUESTS {
                if attempt >= 4 {
                    anyhow::bail!("failed to send a Spotify API request {url}: {text}");
                }
                sleep_rate_limit(attempt, retry_after, &format!("GET {url}")).await;
                attempt += 1;
                continue;
            }

            if status != StatusCode::OK {
                anyhow::bail!("failed to send a Spotify API request {url}: {text}");
            }

            return Ok(serde_json::from_str(&text)?);
        }
    }

    async fn all_paging_items<T>(&self, base_url: &str, count: usize) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        self.all_paging_items_with(base_url, count, &[]).await
    }

    async fn all_paging_items_with<T>(
        &self,
        base_url: &str,
        mut count: usize,
        extra_params: &[(&'static str, &'static str)],
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        const PAGE_LIMIT: usize = 50;
        const MAX_PARALLEL: usize = 8;

        let mut all_items = Vec::new();
        let mut offset = 0;

        // if count is 0 (i.e., unknown), set it to usize::MAX to fetch until no more items
        if count == 0 {
            count = usize::MAX;
        }

        while offset < count {
            let n_jobs = std::cmp::min(MAX_PARALLEL, (count - offset).div_ceil(PAGE_LIMIT));

            let mut futures = Vec::with_capacity(n_jobs);

            for i in 0..n_jobs {
                let current_offset = offset + i * PAGE_LIMIT;
                let limit_str = PAGE_LIMIT.to_string();
                let offset_str = current_offset.to_string();
                let extra_params = extra_params.to_vec();

                futures.push(async move {
                    let params = paging_query(&limit_str, &offset_str, &extra_params);
                    self.http_get::<rspotify::model::Page<T>>(base_url, &params)
                        .await
                });
            }

            let results = futures::future::try_join_all(futures).await?;

            let mut found_empty = false;
            for mut page in results {
                if page.items.is_empty() {
                    found_empty = true;
                    break;
                }
                all_items.append(&mut page.items);
            }

            if found_empty {
                break;
            }

            offset += n_jobs * PAGE_LIMIT;
        }

        Ok(all_items)
    }

    /// Get all cursor-based paging items starting from a pagination object of the first page
    async fn all_cursor_based_paging_items<T>(
        &self,
        first_page: rspotify::model::CursorBasedPage<T>,
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut items = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self
                .http_get::<rspotify::model::CursorBasedPage<T>>(&url, &Query::new())
                .await?;
            items.append(&mut next_page.items);
            maybe_next = next_page.next;
        }
        Ok(items)
    }

    pub async fn current_playback2(
        &self,
    ) -> Result<Option<rspotify::model::CurrentPlaybackContext>> {
        Ok(self.current_playback(None, PLAYBACK_TYPES.into()).await?)
    }

    /// Retrieve the latest playback state
    pub async fn retrieve_current_playback(
        &self,
        state: &SharedState,
        reset_buffered_playback: bool,
    ) -> Result<()> {
        let new_playback = {
            // update the playback state (retry on HTTP 429)
            let playback = {
                let mut attempt = 0u32;
                loop {
                    match self.current_playback2().await {
                        Ok(playback) => break playback,
                        Err(err) if is_rate_limit_msg(&err) && attempt < 4 => {
                            sleep_rate_limit(attempt, None, "current playback").await;
                            attempt += 1;
                        }
                        Err(err) => return Err(err),
                    }
                }
            };
            let playback = overlay_desktop_mpris(playback);
            let mut player = state.player.write();

            let prev_item = player.currently_playing();

            let prev_name = match prev_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            player.playback = playback;
            player.playback_last_updated_time = Some(std::time::Instant::now());

            let curr_item = player.currently_playing();

            let curr_name = match curr_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            let new_playback = prev_name != curr_name && !curr_name.is_empty();
            // check if we need to update the buffered playback
            let needs_update = match (&player.buffered_playback, &player.playback) {
                (Some(bp), Some(p)) => bp.device_id != p.device.id || new_playback,
                (None, None) => false,
                _ => true,
            };

            if reset_buffered_playback || needs_update {
                player.buffered_playback = player.playback.as_ref().map(|p| {
                    let mut playback = PlaybackMetadata::from_playback(p);

                    // handle additional data from the previous buffered state
                    // that is not available in a standard Spotify playback's state
                    if let Some(bp) = &player.buffered_playback {
                        if let Some(volume) = bp.mute_state {
                            playback.volume = Some(volume);
                        }
                        playback.mute_state = bp.mute_state;
                    }
                    playback
                });
            }

            new_playback
        };

        if !new_playback {
            return Ok(());
        }
        self.handle_new_playback_event(state).await?;

        Ok(())
    }

    // Handle new track event
    async fn handle_new_playback_event(&self, state: &SharedState) -> Result<()> {
        let configs = config::get_config();

        let curr_item = {
            let player = state.player.read();
            let Some(track_or_episode) = player.currently_playing() else {
                return Ok(());
            };
            track_or_episode.clone()
        };

        // retrieve current artist for genres if not in cache
        let curr_artist = match &curr_item {
            rspotify::model::PlayableItem::Track(full_track) => {
                let cached = state
                    .data
                    .read()
                    .caches
                    .genres
                    .contains_key(&full_track.artists[0].name);

                if cached {
                    None
                } else {
                    match &full_track.artists[0].id {
                        Some(id) => self.artist(id.clone()).await.ok(),
                        None => None,
                    }
                }
            }
            rspotify::model::PlayableItem::Episode(_)
            | rspotify::model::PlayableItem::Unknown(_) => None,
        };

        if let Some(artist) = curr_artist {
            if !artist.genres.is_empty() {
                state.data.write().caches.genres.insert(
                    artist.name,
                    artist.genres,
                    *TTL_CACHE_DURATION,
                );
            }
        }

        let url = match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                crate::utils::get_track_album_image_url(track)
                    .ok_or(anyhow::anyhow!("missing image"))?
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                crate::utils::get_episode_show_image_url(episode)
                    .ok_or(anyhow::anyhow!("missing image"))?
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        };

        let filename = (match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                let artist = track
                    .album
                    .artists
                    .first()
                    .map_or("unknown", |a| a.name.as_str());
                let album_id = track.album.id.as_ref().map(rspotify::prelude::Id::id);
                let track_id = track.id.as_ref().map(rspotify::prelude::Id::id);
                format!(
                    "{}-{}-cover-{}.jpg",
                    track.album.name,
                    artist,
                    cover_image_id_prefix(album_id, track_id)
                )
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                format!(
                    "{}-{}-cover-{}.jpg",
                    episode.show.name,
                    episode.show.publisher,
                    // first 6 characters of the show's id
                    &episode.show.id.as_ref().id()[..6]
                )
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        })
        .replace('/', ""); // remove invalid characters from the file's name
        let path = configs.cache_folder.join("image").join(filename);

        if configs.app_config.enable_cover_image_cache {
            self.retrieve_image(url, &path, true).await?;
        }

        #[cfg(feature = "image")]
        if !state.data.read().caches.images.contains_key(url) {
            let bytes = self.retrieve_image(url, &path, false).await?;

            #[cfg(not(feature = "pixelate"))]
            let image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;
            #[cfg(feature = "pixelate")]
            let mut image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;

            #[cfg(feature = "pixelate")]
            {
                Self::pixelate_image(&mut image);
            }

            state
                .data
                .write()
                .caches
                .images
                .insert(url.to_owned(), image, *TTL_CACHE_DURATION);
        }

        // notify user about the playback's change if any
        #[cfg(all(feature = "notify", feature = "streaming"))]
        if configs.app_config.enable_notify
            && (!configs.app_config.notify_streaming_only || self.stream_conn.lock().is_some())
        {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        #[cfg(all(feature = "notify", not(feature = "streaming")))]
        if configs.app_config.enable_notify {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        Ok(())
    }

    /// Create a new playlist
    async fn create_new_playlist(
        &self,
        state: &SharedState,
        user_id: UserId<'static>,
        playlist_name: &str,
        public: bool,
        collab: bool,
        desc: &str,
    ) -> Result<()> {
        let playlist: Playlist = self
            .user_playlist_create(
                user_id,
                playlist_name,
                Some(public),
                Some(collab),
                Some(desc),
            )
            .await?
            .into();
        tracing::info!(
            "new playlist (name={},id={}) was successfully created",
            playlist.name,
            playlist.id
        );
        state
            .data
            .write()
            .user_data
            .playlists
            .insert(0, PlaylistFolderItem::Playlist(playlist));
        Ok(())
    }

    #[cfg(feature = "notify")]
    /// Create a notification for a new playback
    fn notify_new_playback(
        playable: &rspotify::model::PlayableItem,
        cover_img_path: &std::path::Path,
    ) -> Result<()> {
        let mut n = notify_rust::Notification::new();

        let re = regex::Regex::new(r"\{.*?\}").unwrap();
        // Generate a text described a track from a format string.
        // For example, a format string "{track} - {artists}" will generate
        // a text consisting of the track's name followed by a dash then artists' names.
        let get_text_from_format_str = |format_str: &str| {
            let mut text = String::new();

            let mut ptr = 0;
            for m in re.find_iter(format_str) {
                let s = m.start();
                let e = m.end();

                if ptr < s {
                    text += &format_str[ptr..s];
                }
                ptr = e;
                match m.as_str() {
                    "{track}" => {
                        let name = match playable {
                            rspotify::model::PlayableItem::Track(ref track) => &track.name,
                            rspotify::model::PlayableItem::Episode(ref episode) => &episode.name,
                            rspotify::model::PlayableItem::Unknown(_) => continue,
                        };
                        text += name;
                    }
                    "{artists}" => {
                        if let rspotify::model::PlayableItem::Track(ref track) = playable {
                            text += &crate::utils::map_join(&track.artists, |a| &a.name, ", ");
                        }
                    }
                    "{album}" => match playable {
                        rspotify::model::PlayableItem::Track(ref track) => {
                            text += &track.album.name;
                        }
                        rspotify::model::PlayableItem::Episode(ref episode) => {
                            text += &episode.show.name;
                        }
                        rspotify::model::PlayableItem::Unknown(_) => {}
                    },
                    &_ => {}
                }
            }
            if ptr < format_str.len() {
                text += &format_str[ptr..];
            }

            text
        };

        let configs = config::get_config();

        n.appname("spotify_player")
            .summary(&get_text_from_format_str(
                &configs.app_config.notify_format.summary,
            ))
            .body(&get_text_from_format_str(
                &configs.app_config.notify_format.body,
            ));
        if cover_img_path.exists() {
            n.icon(cover_img_path.to_str().context("valid cover_img_path")?);
        }
        if configs.app_config.notify_timeout_in_secs > 0 {
            n.timeout(std::time::Duration::from_secs(
                configs.app_config.notify_timeout_in_secs,
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        if configs.app_config.notify_transient {
            use notify_rust::Hint;
            n.hint(Hint::Transient(true));
        }
        n.show()?;

        Ok(())
    }

    /// Retrieve an image from a `url` or a cached `path`.
    /// If `saved` is specified, the retrieved image is saved to the cached `path`.
    async fn retrieve_image(
        &self,
        url: &str,
        path: &std::path::Path,
        saved: bool,
    ) -> Result<Vec<u8>> {
        if path.exists() {
            tracing::debug!("Retrieving image from file: {}", path.display());
            return Ok(std::fs::read(path)?);
        }

        tracing::info!("Retrieving image from url: {url}");

        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("get image from url {url}"))?
            .bytes()
            .await?;

        if saved {
            tracing::info!("Saving the retrieved image into {}", path.display());
            let mut file = std::fs::File::create(path)?;
            file.write_all(&bytes)?;
        }

        Ok(bytes.to_vec())
    }

    #[cfg(feature = "pixelate")]
    fn pixelate_image(image: &mut image::DynamicImage) {
        let pixels = config::get_config().app_config.cover_img_pixels;
        let pixelated_image = image.resize(pixels, pixels, image::imageops::FilterType::Nearest);
        *image = pixelated_image.resize(
            image.width(),
            image.height(),
            image::imageops::FilterType::Nearest,
        );
    }

    /// Process a list of albums, which includes
    /// - sort albums by the release date
    /// - sort albums by the type if `sort_artist_albums_by_type` config is enabled
    fn process_artist_albums(mut albums: Vec<Album>) -> Vec<Album> {
        albums.sort_by(|x, y| y.release_date.partial_cmp(&x.release_date).unwrap());

        if config::get_config().app_config.sort_artist_albums_by_type {
            fn get_priority(album_type: &str) -> usize {
                match album_type {
                    "album" => 0,
                    "single" => 1,
                    "appears_on" => 2,
                    "compilation" => 3,
                    _ => 4,
                }
            }
            albums.sort_by_key(|a| get_priority(&a.album_type()));
        }

        albums
    }
}

fn move_seed_track_to_front(tracks: &mut Vec<Track>, seed_track: Track) {
    tracks.retain(|track| track.id != seed_track.id);
    tracks.insert(0, seed_track);
}

fn is_rate_limit_msg(err: &impl std::fmt::Display) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("429")
        || msg.contains("too many requests")
        || msg.contains("api rate limit exceeded")
        || msg.contains("rate limit")
}

fn rate_limit_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(4)).min(Duration::from_secs(30))
}

async fn sleep_rate_limit(attempt: u32, retry_after_secs: Option<u64>, context: &str) {
    let wait = retry_after_secs
        .map_or_else(|| rate_limit_backoff(attempt), Duration::from_secs)
        .min(MAX_RETRY_AFTER);
    tracing::warn!(
        "Spotify API rate limited ({context}); backing off {wait:?} (attempt {})",
        attempt + 1
    );
    tokio::time::sleep(wait).await;
}

fn parse_retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Overlay Linux desktop MPRIS onto Connect playback:
/// - no Connect session → show the MPRIS track so the window is not empty
/// - Connect session on `preferred_device` with volume 0/None → use MPRIS volume
///   (the Web API often reports 0% for the official client while audio is fine)
fn overlay_desktop_mpris(
    connect: Option<rspotify::model::CurrentPlaybackContext>,
) -> Option<rspotify::model::CurrentPlaybackContext> {
    #[cfg(not(target_os = "linux"))]
    {
        connect
    }
    #[cfg(target_os = "linux")]
    {
        let configs = config::get_config();
        let desktop = &configs.app_config.desktop_spotify;
        if !desktop.enable {
            return connect;
        }
        let device_name = configs
            .app_config
            .preferred_device
            .clone()
            .unwrap_or_else(|| "Spotify".to_string());
        match connect {
            None => {
                match crate::desktop_spotify::current_playback_from_mpris(
                    &desktop.mpris_dest,
                    &device_name,
                ) {
                    Ok(Some(playback)) => {
                        tracing::debug!(
                            "Connect has no current playback; showing desktop MPRIS track as {device_name}"
                        );
                        Some(playback)
                    }
                    Ok(None) => None,
                    Err(err) => {
                        tracing::debug!("MPRIS playback fallback failed: {err:#}");
                        None
                    }
                }
            }
            Some(mut playback) => {
                if playback.device.name.eq_ignore_ascii_case(&device_name) {
                    let mpris = crate::desktop_spotify::mpris_volume_percent(&desktop.mpris_dest);
                    let overlaid = crate::desktop_spotify::overlay_connect_volume(
                        playback.device.volume_percent,
                        mpris,
                    );
                    if overlaid != playback.device.volume_percent {
                        tracing::debug!(
                            "Connect volume {:?} is silent for {device_name}; using MPRIS volume {overlaid:?}",
                            playback.device.volume_percent
                        );
                        playback.device.volume_percent = overlaid;
                    }
                }
                Some(playback)
            }
        }
    }
}

fn cover_image_id_prefix(album_id: Option<&str>, track_id: Option<&str>) -> String {
    let src = album_id.or(track_id).unwrap_or("mpris");
    src.chars().take(6).collect()
}

/// Wake the desktop client, returning how the wake went (`None` when it did not happen).
#[cfg(target_os = "linux")]
async fn wake_desktop_spotify_if_enabled(
    client: &AppClient,
    state: &SharedState,
) -> Option<crate::desktop_spotify::WakeOutcome> {
    let desktop = config::get_config().app_config.desktop_spotify.clone();
    if !desktop.enable {
        return None;
    }

    let will_launch = match crate::desktop_spotify::will_launch(&desktop) {
        Ok(will_launch) => will_launch,
        Err(err) => {
            tracing::warn!("Failed to inspect desktop Spotify state: {err:#}");
            state.push_error_toast(format!("Could not inspect Spotify desktop: {err:#}"));
            return None;
        }
    };

    if will_launch {
        state.push_success_toast(
            "Preferred device unavailable — starting Spotify desktop automatically…",
        );
    } else {
        state.push_success_toast("Waking Spotify desktop…");
    }

    let recent_uri = match client.current_user_recently_played_tracks().await {
        Ok(tracks) => tracks.first().map(|t| t.id.uri()),
        Err(err) => {
            tracing::warn!("Failed to fetch recently played for desktop wake URI: {err:#}");
            None
        }
    };

    let nudge_uri = crate::desktop_spotify::resolve_nudge_uri(
        desktop.nudge_uri.as_deref(),
        recent_uri.as_deref(),
    );

    match crate::desktop_spotify::ensure_awake(&desktop, nudge_uri.as_deref()).await {
        Ok(outcome) => {
            let message = match (outcome.launched, outcome.minimized) {
                (true, true) => "Spotify desktop started in the system tray and is ready",
                (true, false) => "Spotify desktop started and is ready",
                (false, _) => "Spotify desktop is ready",
            };
            tracing::info!("{message}");
            state.push_success_toast(message);
            Some(outcome)
        }
        Err(err) => {
            tracing::warn!("Desktop Spotify wake failed: {err:#}");
            state.push_error_toast(format!("Could not start Spotify desktop: {err:#}"));
            None
        }
    }
}

/// Whether playback init should launch/nudge the official desktop Spotify client.
///
/// - With `preferred_device`: wake when that name is absent from Connect (even if
///   other devices like smart speakers are listed), **or** when it is listed but
///   not actively playing (typical idle/paused tray/autostart client). Do not nudge
///   merely to steal from another device that is already playing while preferred is listed.
/// - Without `preferred_device`: wake only when the transferable device list is empty.
#[cfg(target_os = "linux")]
async fn desktop_wake_needed(
    client: &AppClient,
    preferred_actively_playing: bool,
    other_actively_playing: bool,
) -> Result<bool> {
    let configs = config::get_config();
    if !configs.app_config.desktop_spotify.enable {
        return Ok(false);
    }

    let preferred = configs
        .app_config
        .preferred_device
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(name) = preferred {
        let devices = client.available_devices().await?;
        let pairs = devices
            .iter()
            .filter_map(|d| Some((d.id.as_deref()?, d.name.as_str())));
        let present = preferred_device_id(pairs, name).is_some();
        let needed =
            should_wake_for_preferred(present, preferred_actively_playing, other_actively_playing);
        if needed {
            if present {
                tracing::info!(
                    "Preferred Connect device `{name}` is listed but idle; will nudge desktop Spotify"
                );
            } else {
                tracing::info!(
                    "Preferred Connect device `{name}` not listed; will wake desktop Spotify"
                );
            }
        }
        Ok(needed)
    } else {
        let ids = client.find_available_device_ids().await?;
        Ok(ids.is_empty())
    }
}

/// Decide whether a preferred desktop Connect endpoint still needs an MPRIS wake.
///
/// Autostart often leaves the official client in the tray with the preferred name
/// already registered and paused. Skipping the nudge then leaves Connect/TUI stuck
/// until the user hits play in the GUI.
#[cfg(any(test, target_os = "linux"))]
fn should_wake_for_preferred(
    preferred_present: bool,
    preferred_actively_playing: bool,
    other_actively_playing: bool,
) -> bool {
    if !preferred_present {
        return true;
    }
    if preferred_actively_playing {
        return false;
    }
    // Listed but not playing on preferred. Nudge idle/paused tray clients, but do
    // not steal when another speaker already owns active audio.
    !other_actively_playing
}

/// Find the Connect id of `preferred` among `devices`, given as `(id, name)` pairs.
///
/// Compiled on Linux (production wake path) and under `cfg(test)` so unit tests
/// can exercise matching on every CI host. The non-test macOS/Windows binary
/// never calls this helper.
#[cfg(any(test, target_os = "linux"))]
fn preferred_device_id<'a>(
    devices: impl IntoIterator<Item = (&'a str, &'a str)>,
    preferred: &str,
) -> Option<&'a str> {
    devices
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(preferred))
        .map(|(id, _)| id)
}

/// Poll Connect until the woken desktop client registers, returning its device id.
///
/// Spotify lists the freshly woken client a few seconds after it starts playing.
/// Until then the API still reports the previously active speaker, so transferring
/// to whatever is "active" right now would pull audio back off the desktop.
#[cfg(target_os = "linux")]
async fn wait_for_preferred_device(client: &AppClient, state: &SharedState) -> Option<String> {
    wait_for_preferred_device_with(client, state, PREFERRED_DEVICE_WAIT, true).await
}

#[cfg(target_os = "linux")]
async fn wait_for_preferred_device_with(
    client: &AppClient,
    state: &SharedState,
    timeout: Duration,
    report_timeout: bool,
) -> Option<String> {
    let preferred = config::get_config()
        .app_config
        .preferred_device
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())?
        .to_string();

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match client.available_devices().await {
            Ok(devices) => {
                let pairs = devices
                    .iter()
                    .filter_map(|d| Some((d.id.as_deref()?, d.name.as_str())));
                if let Some(id) = preferred_device_id(pairs, &preferred) {
                    tracing::info!("Woken device `{preferred}` registered with Connect (id={id})");
                    return Some(id.to_string());
                }
            }
            Err(err) => tracing::warn!("Failed to list devices after desktop wake: {err:#}"),
        }

        if std::time::Instant::now() >= deadline {
            if report_timeout {
                tracing::warn!("Woke Spotify desktop but `{preferred}` never appeared in Connect");
                state.push_error_toast(format!(
                    "Spotify desktop is running, but `{preferred}` did not appear in Connect"
                ));
            } else {
                tracing::info!(
                    "Desktop Spotify is playing locally, but `{preferred}` is not listed in Connect; leaving playback unchanged"
                );
            }
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

fn should_select_device(has_playback: bool, woke_desktop: bool) -> bool {
    !has_playback || woke_desktop
}

/// After a desktop wake, transfer audio onto the new device only when the
/// wake is allowed to keep playing. Startup launches still need a Play/OpenUri
/// nudge so Connect can see the client, then pause unless the user asked to
/// keep that automatically started playback.
///
/// When `local_already_playing` is true and preferred was targeted, the transfer
/// keeps playing even if `pause_after_nudge` is true.
fn keep_playing_after_desktop_wake(
    woke_preferred: bool,
    pause_after_nudge: bool,
    local_already_playing: bool,
) -> bool {
    if local_already_playing && woke_preferred {
        return true;
    }
    woke_preferred && !pause_after_nudge
}

/// Select transfer candidates after attempting a desktop wake.
///
/// A successful wake for a preferred device must either target that device or
/// leave playback unchanged. Falling back to generic active devices can move
/// playback to an unrelated speaker while the preferred device is still
/// registering with Connect.
#[derive(Debug, PartialEq, Eq)]
enum DeviceIdsAfterWake {
    Preferred(Vec<String>),
    SkipTransfer,
    GenericFallback,
}

fn device_ids_after_wake(
    wake_target: Option<String>,
    woke_for_preferred: bool,
) -> DeviceIdsAfterWake {
    match (woke_for_preferred, wake_target) {
        (_, Some(id)) => DeviceIdsAfterWake::Preferred(vec![id]),
        (true, None) => DeviceIdsAfterWake::SkipTransfer,
        (false, None) => DeviceIdsAfterWake::GenericFallback,
    }
}

/// Order Connect device IDs for transfer attempts.
///
/// Prefer `preferred_name`, then non-integrated devices, then (optionally) the
/// integrated librespot device. When streaming is disabled, integrated devices
/// are omitted entirely — transferring to them returns HTTP 404.
fn order_transfer_device_ids(
    devices: &[Device],
    preferred_name: Option<&str>,
    include_integrated: bool,
) -> Vec<String> {
    let preferred_name = preferred_name.map(str::trim).filter(|s| !s.is_empty());

    let mut ids = Vec::new();
    let mut push_unique = |id: &str| {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    };

    if let Some(name) = preferred_name {
        for device in devices.iter().filter(|d| d.name.eq_ignore_ascii_case(name)) {
            if include_integrated || !device.is_integrated {
                push_unique(&device.id);
            }
        }
    }

    for device in devices.iter().filter(|d| !d.is_integrated) {
        push_unique(&device.id);
    }

    if include_integrated {
        for device in devices.iter().filter(|d| d.is_integrated) {
            push_unique(&device.id);
        }
    }

    ids
}

/// Patch Spotify API JSON so rspotify 0.15 can deserialize responses that omit
/// fields Spotify no longer always returns (notably `available_markets` on shows).
fn process_spotify_api_response(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut value) => {
            patch_missing_show_fields(&mut value);
            value.to_string()
        }
        Err(_) => text.to_string(),
    }
}

fn patch_missing_show_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let looks_like_show = map.contains_key("media_type")
                && map.contains_key("name")
                && (map.contains_key("publisher")
                    || map.contains_key("languages")
                    || map.contains_key("episodes"));
            if looks_like_show && !map.contains_key("available_markets") {
                map.insert(
                    "available_markets".to_string(),
                    serde_json::Value::Array(Vec::new()),
                );
            }
            for child in map.values_mut() {
                patch_missing_show_fields(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                patch_missing_show_fields(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_memory_caches_on_new_session, cover_image_id_prefix, device_ids_after_wake,
        keep_playing_after_desktop_wake, move_seed_track_to_front, order_transfer_device_ids,
        paging_query, preferred_device_id, process_spotify_api_response, rate_limit_backoff,
        should_select_device, should_wake_for_preferred, top_tracks_time_range_param,
        DeviceIdsAfterWake, MAX_RETRY_AFTER,
    };
    use crate::state::{Device, Track};
    use rspotify::model::TrackId;

    fn sample_track(id: &'static str, name: &str) -> Track {
        Track {
            id: TrackId::from_id(id).unwrap().into_static(),
            name: name.to_string(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::default(),
            explicit: false,
            added_at: 0,
        }
    }

    #[test]
    fn move_seed_track_to_front_prepends_missing_seed() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let third = sample_track("1301WleyT98MSxVHPZCA6M", "third");
        let mut tracks = vec![second.clone(), third];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn move_seed_track_to_front_reorders_existing_seed_without_duplication() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let mut tracks = vec![second.clone(), seed.clone()];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn order_devices_skips_integrated_when_streaming_disabled() {
        let devices = vec![
            Device {
                id: "integrated".into(),
                name: "spotify-player".into(),
                is_integrated: true,
            },
            Device {
                id: "desktop".into(),
                name: "estelle".into(),
                is_integrated: false,
            },
            Device {
                id: "everywhere".into(),
                name: "Everywhere".into(),
                is_integrated: false,
            },
        ];

        assert_eq!(
            order_transfer_device_ids(&devices, Some("estelle"), false),
            vec!["desktop".to_string(), "everywhere".to_string()]
        );
    }

    #[test]
    fn order_devices_puts_integrated_last_when_streaming_enabled() {
        let devices = vec![
            Device {
                id: "integrated".into(),
                name: "spotify-player".into(),
                is_integrated: true,
            },
            Device {
                id: "desktop".into(),
                name: "estelle".into(),
                is_integrated: false,
            },
        ];

        assert_eq!(
            order_transfer_device_ids(&devices, None, true),
            vec!["desktop".to_string(), "integrated".to_string()]
        );
    }

    #[test]
    fn other_active_devices_do_not_satisfy_preferred_device() {
        assert_eq!(
            preferred_device_id([("echo-id", "Everywhere")], "estelle"),
            None
        );
    }

    #[test]
    fn preferred_device_id_matches_name_case_insensitively() {
        assert_eq!(
            preferred_device_id(
                [("echo-id", "Everywhere"), ("desktop-id", "ESTELLE")],
                "estelle"
            ),
            Some("desktop-id")
        );
    }

    #[test]
    fn successful_desktop_wake_forces_selection_despite_existing_playback() {
        assert!(!should_select_device(true, false));
        assert!(should_select_device(true, true));
        assert!(should_select_device(false, false));
    }

    #[test]
    fn desktop_wake_does_not_keep_playing_when_pause_after_nudge() {
        assert!(!keep_playing_after_desktop_wake(true, true, false));
        assert!(keep_playing_after_desktop_wake(true, false, false));
        assert!(!keep_playing_after_desktop_wake(false, false, false));
        assert!(!keep_playing_after_desktop_wake(false, true, false));
        // Local MPRIS already Playing: transfer must not pause.
        assert!(keep_playing_after_desktop_wake(true, true, true));
        assert!(!keep_playing_after_desktop_wake(false, true, true));
    }

    #[test]
    fn cover_image_id_prefix_falls_back_without_album_id() {
        assert_eq!(cover_image_id_prefix(Some("abcdef123"), None), "abcdef");
        assert_eq!(
            cover_image_id_prefix(None, Some("6lmsHxA47XsTQ1BPL1PMx7")),
            "6lmsHx"
        );
        assert_eq!(cover_image_id_prefix(None, None), "mpris");
    }

    #[test]
    fn idle_preferred_device_still_needs_mpris_nudge() {
        // Missing from Connect → always wake (even if another speaker is playing).
        assert!(should_wake_for_preferred(false, false, false));
        assert!(should_wake_for_preferred(false, false, true));
        // Listed + actively playing on preferred → leave alone.
        assert!(!should_wake_for_preferred(true, true, false));
        // Listed + paused/idle → nudge the tray client.
        assert!(should_wake_for_preferred(true, false, false));
        // Listed while another device is actively playing → do not steal.
        assert!(!should_wake_for_preferred(true, false, true));
    }

    #[test]
    fn preferred_device_wake_timeout_skips_generic_transfer_candidates() {
        assert_eq!(
            device_ids_after_wake(None, true),
            DeviceIdsAfterWake::SkipTransfer
        );
        assert_eq!(
            device_ids_after_wake(None, false),
            DeviceIdsAfterWake::GenericFallback
        );
    }

    #[test]
    fn process_spotify_api_response_fills_missing_available_markets() {
        let raw = r#"{
            "items": [{
                "added_at": "2024-01-01T00:00:00Z",
                "show": {
                    "copyrights": [],
                    "description": "desc",
                    "explicit": false,
                    "external_urls": {},
                    "href": "https://api.spotify.com/v1/shows/abc",
                    "id": "abc",
                    "images": [],
                    "is_externally_hosted": false,
                    "languages": ["en"],
                    "media_type": "audio",
                    "name": "A Show",
                    "publisher": "Pub",
                    "type": "show",
                    "uri": "spotify:show:abc",
                    "total_episodes": 1
                }
            }],
            "total": 1,
            "limit": 50,
            "offset": 0,
            "href": "https://api.spotify.com/v1/me/shows",
            "next": null,
            "previous": null
        }"#;

        let patched: serde_json::Value =
            serde_json::from_str(&process_spotify_api_response(raw)).unwrap();
        let markets = &patched["items"][0]["show"]["available_markets"];
        assert!(markets.is_array());
        assert!(markets.as_array().unwrap().is_empty());
    }

    #[test]
    fn top_tracks_time_range_param_uses_spotify_snake_case() {
        assert_eq!(
            top_tracks_time_range_param(rspotify::model::TimeRange::ShortTerm),
            ("time_range", "short_term")
        );
        assert_eq!(
            top_tracks_time_range_param(rspotify::model::TimeRange::MediumTerm),
            ("time_range", "medium_term")
        );
        assert_eq!(
            top_tracks_time_range_param(rspotify::model::TimeRange::LongTerm),
            ("time_range", "long_term")
        );
    }

    #[test]
    fn paging_query_includes_time_range_extra_on_every_page() {
        let extra = [top_tracks_time_range_param(
            rspotify::model::TimeRange::MediumTerm,
        )];
        let first = paging_query("50", "0", &extra);
        let second = paging_query("50", "50", &extra);

        assert_eq!(first.get("market").copied(), Some("from_token"));
        assert_eq!(first.get("limit").copied(), Some("50"));
        assert_eq!(first.get("offset").copied(), Some("0"));
        assert_eq!(first.get("time_range").copied(), Some("medium_term"));

        assert_eq!(second.get("offset").copied(), Some("50"));
        assert_eq!(second.get("time_range").copied(), Some("medium_term"));
    }

    #[test]
    fn paging_query_omits_time_range_without_extras() {
        let params = paging_query("50", "0", &[]);
        assert!(!params.contains_key("time_range"));
        assert_eq!(params.get("market").copied(), Some("from_token"));
    }

    #[test]
    fn rate_limit_backoff_is_bounded() {
        assert_eq!(rate_limit_backoff(0).as_secs(), 1);
        assert_eq!(rate_limit_backoff(4).as_secs(), 16);
        assert_eq!(rate_limit_backoff(10).as_secs(), 16);
        // Oversized Retry-After values must still be capped before sleep.
        assert!(MAX_RETRY_AFTER.as_secs() <= 60);
        assert!(
            std::time::Duration::from_secs(u64::MAX)
                .min(MAX_RETRY_AFTER)
                .as_secs()
                <= 60
        );
    }

    #[test]
    fn reconnect_keeps_memory_caches() {
        assert!(!clear_memory_caches_on_new_session(false));
        assert!(clear_memory_caches_on_new_session(true));
    }

    #[test]
    fn next_repeat_state_is_context_then_track_then_off() {
        use rspotify::model::RepeatState::{Context, Off, Track};
        assert_eq!(super::next_repeat_state(Context), Track);
        assert_eq!(super::next_repeat_state(Track), Off);
        assert_eq!(super::next_repeat_state(Off), Context);
    }
}
