use std::time::{Duration, Instant};

use anyhow::Context;
use rspotify::model::Id;
use tracing::Instrument;

use crate::{
    config,
    state::{ContextId, ContextPageType, ContextPageUIState, PageState, PlayableId, SharedState},
};

use crate::utils::map_join;

use super::ClientRequest;

struct PlayerEventHandlerState {
    get_context_timer: Instant,
    last_playback_refresh_timer: Instant,
    /// Last time we enqueued a track-end `GetCurrentPlayback` (debounce stampede).
    last_track_end_fetch: Instant,
    /// Last time we enqueued a `GetCurrentUserQueue` from the watcher.
    last_queue_fetch: Instant,
}

/// Cap how long any single client request may block the handler / a worker task.
/// Without this, a hung Spotify HTTP call (or oversized Retry-After sleep) can wedge
/// the TUI command path and the CLI UDP socket indefinitely.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum gap between track-end playback refreshes. The watcher runs every 100ms and
/// used to enqueue `GetCurrentPlayback` on every tick once progress >= duration, which
/// stampedes the API when a fetch is slow or hung.
const TRACK_END_FETCH_INTERVAL: Duration = Duration::from_secs(2);

/// Minimum gap between watcher-driven queue refreshes (missing/mismatched queue).
const QUEUE_FETCH_INTERVAL: Duration = Duration::from_secs(5);

/// When `enable_streaming = Never`, external Connect clients (desktop/phone) can
/// change track/device without local librespot events. Event-only refresh (`0`)
/// then leaves the TUI stuck on a stale song until manual `Ctrl-R`. Use a light
/// poll as the Connect-mode fallback; set an explicit positive
/// `playback_refresh_duration_in_ms` to override, or a large value if you truly
/// want event-only behavior with streaming disabled.
const CONNECT_MODE_PLAYBACK_REFRESH_FALLBACK: Duration = Duration::from_secs(5);

/// starts the client's request handler
pub async fn start_client_handler(
    state: &SharedState,
    client: &super::AppClient,
    client_sub: &flume::Receiver<ClientRequest>,
) {
    while let Ok(request) = client_sub.recv_async().await {
        let state = state.clone();
        let client = client.clone();
        let span = tracing::info_span!("client_request", request = ?request);

        // Player mutations read and write `buffered_playback`; run them serially so
        // rapid repeat/shuffle/etc. keys cannot race on stale state.
        // Bound the wait so a single hung Player API call cannot stall the loop forever.
        if matches!(&request, ClientRequest::Player(_)) {
            match tokio::time::timeout(
                CLIENT_REQUEST_TIMEOUT,
                client.handle_request(&state, request).instrument(span),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!("Failed to handle client request: {err:#}"),
                Err(_) => tracing::error!(
                    "Timed out after {CLIENT_REQUEST_TIMEOUT:?} handling Player client request"
                ),
            }
        } else {
            tokio::task::spawn(
                async move {
                    match tokio::time::timeout(
                        CLIENT_REQUEST_TIMEOUT,
                        client.handle_request(&state, request),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            tracing::error!("Failed to handle client request: {err:#}");
                        }
                        Err(_) => {
                            tracing::error!(
                                "Timed out after {CLIENT_REQUEST_TIMEOUT:?} handling client request"
                            );
                        }
                    }
                }
                .instrument(span),
            );
        }
    }
}

/// Interval between background session-validity checks.
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub async fn start_session_watcher(state: SharedState, client: super::AppClient) {
    let mut interval = tokio::time::interval(SESSION_CHECK_INTERVAL);
    // If a check ever runs long (e.g. a slow reconnect), skip missed ticks
    // rather than firing them back-to-back.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        if let Err(err) = client.check_valid_session(&state).await {
            tracing::error!("Failed to check/reconnect the client's session: {err:#}");
        }
    }
}

fn handle_playback_change_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    let player = state.player.read();
    let (playback, id, duration) = match (
        player.buffered_playback.as_ref(),
        player.currently_playing(),
    ) {
        (Some(playback), Some(rspotify::model::PlayableItem::Track(track))) => (
            playback,
            PlayableId::Track(track.id.clone().expect("null track_id")),
            track.duration,
        ),
        (Some(playback), Some(rspotify::model::PlayableItem::Episode(episode))) => (
            playback,
            PlayableId::Episode(episode.id.clone()),
            episode.duration,
        ),
        _ => return Ok(()),
    };

    if let Some(progress) = player.playback_progress() {
        // Update playback when the current track ends. Debounce: the watcher ticks
        // every 100ms and must not enqueue a request on every tick while waiting.
        if progress >= duration
            && playback.is_playing
            && handler_state.last_track_end_fetch.elapsed() >= TRACK_END_FETCH_INTERVAL
        {
            client_pub.send(ClientRequest::GetCurrentPlayback)?;
            handler_state.last_track_end_fetch = Instant::now();
        }
    }

    let needs_queue_fetch = match player.queue.as_ref() {
        Some(queue) => queue
            .currently_playing
            .as_ref()
            .is_some_and(|queue_track| queue_track.id().expect("null track_id") != id),
        None => true,
    };
    if needs_queue_fetch && handler_state.last_queue_fetch.elapsed() >= QUEUE_FETCH_INTERVAL {
        client_pub.send(ClientRequest::GetCurrentUserQueue)?;
        handler_state.last_queue_fetch = Instant::now();
    }

    Ok(())
}

fn handle_page_change_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    // Never hold `ui` across `player`/`data` locks — the UI thread takes `ui` then
    // `player`/`vis_bands` during draw; inverted or overlapping orders freeze the TUI.
    let (playing_context_id, playing_track) = {
        let player = state.player.read();
        let track = player.currently_playing().and_then(|item| {
            if let rspotify::model::PlayableItem::Track(track) = item {
                Some((
                    track.name.clone(),
                    map_join(&track.artists, |a| &a.name, ", "),
                    track.id.clone(),
                ))
            } else {
                None
            }
        });
        (player.playing_context_id(), track)
    };

    let mut context_to_fetch = None;

    {
        let mut ui = state.ui.lock();
        match ui.current_page_mut() {
            PageState::Context {
                id,
                context_page_type,
                state: page_state,
            } => {
                let expected_id = match context_page_type {
                    ContextPageType::Browsing(context_id) => Some(context_id.clone()),
                    ContextPageType::CurrentPlaying => playing_context_id,
                };

                let new_id = if *id == expected_id {
                    false
                } else {
                    tracing::info!(
                        "Current context ID ({:?}) is different from the expected ID ({:?}), update the context state",
                        id,
                        expected_id
                    );

                    *id = expected_id;

                    match id {
                        Some(id) => {
                            *page_state = Some(ContextPageUIState::from_id(id));
                        }
                        None => {
                            *page_state = None;
                        }
                    }
                    true
                };

                // Candidate for GetContext when id changed or refresh interval elapsed.
                // Cache check happens after releasing `ui` (lock-order: never hold ui across data).
                if let Some(id) = id {
                    if !matches!(id, ContextId::Tracks(_))
                        && (new_id
                            || handler_state.get_context_timer.elapsed() > Duration::from_secs(5))
                    {
                        context_to_fetch = Some(id.clone());
                    }
                }
            }

            PageState::Lyrics {
                track_uri,
                track,
                artists,
            } => {
                if let Some((name, artist_names, track_id)) = playing_track {
                    if name != *track {
                        if let Some(id) = track_id {
                            tracing::info!(
                                "Currently playing track \"{name}\" is different from the track \"{track}\" shown up in the lyrics page. Fetching new track's lyrics..."
                            );
                            *track = name;
                            *artists = artist_names;
                            *track_uri = id.uri();
                            client_pub.send(ClientRequest::GetLyrics {
                                track_id: id.clone_static(),
                            })?;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Fetch only if missing from cache; (new_id || timer) already selected the candidate.
    if let Some(id) = context_to_fetch {
        if !state.data.read().caches.context.contains_key(&id.uri()) {
            client_pub.send(ClientRequest::GetContext(id))?;
            handler_state.get_context_timer = Instant::now();
        }
    }

    Ok(())
}

fn handle_player_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    handle_page_change_event(state, client_pub, handler_state)
        .context("handle page change event")?;
    handle_playback_change_event(state, client_pub, handler_state)
        .context("handle playback change event")?;

    Ok(())
}

/// Effective playback poll interval for the event watcher.
///
/// `playback_refresh_duration_in_ms > 0` wins. Otherwise, Connect/remote-control
/// mode (`enable_streaming = Never`) falls back to a light poll so external
/// track changes appear without manual refresh.
fn effective_playback_refresh_duration(configs: &config::Configs) -> Option<Duration> {
    let configured_ms = configs.app_config.playback_refresh_duration_in_ms;
    if configured_ms > 0 {
        return Some(Duration::from_millis(configured_ms));
    }
    if configs.app_config.enable_streaming == config::StreamingType::Never {
        return Some(CONNECT_MODE_PLAYBACK_REFRESH_FALLBACK);
    }
    None
}

/// Starts event watcher listening to events and making update requests to the client if needed
pub fn start_player_event_watcher(state: &SharedState, client_pub: &flume::Sender<ClientRequest>) {
    let configs = config::get_config();

    let refresh_duration = Duration::from_millis(100);
    let playback_refresh_duration = effective_playback_refresh_duration(configs);
    // Start elapsed so the first legitimate track-end/queue fetch is not delayed.
    let fetch_epoch = Instant::now()
        .checked_sub(QUEUE_FETCH_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut handler_state = PlayerEventHandlerState {
        get_context_timer: Instant::now(),
        last_playback_refresh_timer: Instant::now(),
        last_track_end_fetch: fetch_epoch,
        last_queue_fetch: fetch_epoch,
    };

    loop {
        // Periodically refresh playback when configured, or when Connect/Never
        // mode needs a fallback poll (external clients change tracks silently).
        if let Some(interval) = playback_refresh_duration {
            if handler_state.last_playback_refresh_timer.elapsed() >= interval {
                client_pub
                    .send(ClientRequest::GetCurrentPlayback)
                    .unwrap_or_default();
                handler_state.last_playback_refresh_timer = Instant::now();
            }
        }

        if let Err(err) = handle_player_event(state, client_pub, &mut handler_state) {
            tracing::error!("Encounter error when handling player event: {err:#}");
        }

        std::thread::sleep(refresh_duration);
    }
}
