use std::time::{Duration, Instant};

use anyhow::Context;
use rspotify::model::Id;
use tracing::Instrument;

use crate::{
    config,
    state::{
        ContextId, ContextPageType, ContextPageUIState, PageState, PlayableId, SharedState,
        RATE_LIMIT_TOAST_PREFIX,
    },
};

use crate::utils::map_join;

use super::{ClientRequest, PlayerRequest};

struct PlayerEventHandlerState {
    get_context_timer: Instant,
    last_playback_refresh_timer: Instant,
    /// Last time we enqueued a track-end `GetCurrentPlayback` (debounce stampede).
    last_track_end_fetch: Instant,
    /// Last time we enqueued a `GetCurrentUserQueue` from the watcher.
    last_queue_fetch: Instant,
}

/// Cap how long any single client request may block a worker task.
/// Without this, a hung Spotify HTTP call (or oversized Retry-After sleep) can wedge
/// the TUI command path and the CLI UDP socket indefinitely.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Player requests may wake the desktop client (`ready_timeout_secs`, default
/// 45 s, plus the 15 s Connect registration wait). They run on their own worker,
/// so this longer bound never delays other requests.
const PLAYER_REQUEST_TIMEOUT: Duration = Duration::from_secs(75);

/// A player command Spotify rejects with `429` is sent once more when the
/// wait it asks for is no longer than this. A longer wait is only reported.
const PLAYER_RETRY_MAX_WAIT: Duration = Duration::from_secs(5);
/// `Retry-After` counts whole seconds; a resend on the dot is rejected again.
const PLAYER_RETRY_MARGIN: Duration = Duration::from_millis(500);

type RequestOutcome = Result<anyhow::Result<()>, tokio::time::error::Elapsed>;

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
    // Player mutations read and write `buffered_playback`; a dedicated worker runs
    // them one at a time so rapid repeat/shuffle keys cannot race on stale state,
    // while this loop keeps draining every other request (a slow player call used
    // to stall playback polls, searches and CLI replies behind it).
    let (player_tx, player_rx) = flume::unbounded::<ClientRequest>();
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            start_player_worker(&state, &client, &player_rx).await;
        }
    });

    while let Ok(request) = client_sub.recv_async().await {
        if matches!(&request, ClientRequest::Player(_)) {
            if player_tx.send(request).is_err() {
                tracing::error!("Player request worker is gone; dropping the request");
            }
            continue;
        }

        let state = state.clone();
        let client = client.clone();
        let span = tracing::info_span!("client_request", request = ?request);
        let toast_request = request.clone();
        tokio::task::spawn(
            async move {
                let outcome = tokio::time::timeout(
                    CLIENT_REQUEST_TIMEOUT,
                    client.handle_request(&state, request),
                )
                .await;
                enqueue_request_toast(&state, &toast_request, outcome, CLIENT_REQUEST_TIMEOUT);
            }
            .instrument(span),
        );
    }
}

async fn start_player_worker(
    state: &SharedState,
    client: &super::AppClient,
    player_sub: &flume::Receiver<ClientRequest>,
) {
    // Requests taken off the channel while a resend was waiting, still to run.
    let mut backlog = std::collections::VecDeque::new();
    loop {
        let request = match backlog.pop_front() {
            Some(request) => request,
            None => match player_sub.recv_async().await {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        let span = tracing::info_span!("player_request", request = ?request);
        let run = || {
            tokio::time::timeout(
                PLAYER_REQUEST_TIMEOUT,
                client
                    .handle_request(state, request.clone())
                    .instrument(span.clone()),
            )
        };

        let mut outcome = run().await;
        if let (ClientRequest::Player(sent), Some(wait)) =
            (&request, short_rate_limit_wait(&outcome))
        {
            state.push_rate_limit_toast(format!(
                "{RATE_LIMIT_TOAST_PREFIX}: sending {} again in {} s",
                sent.action(),
                wait.as_secs().max(1)
            ));
            tokio::time::sleep(wait + PLAYER_RETRY_MARGIN).await;
            backlog.extend(player_sub.drain());
            backlog.retain(|queued| !repeats_toggle(queued, sent));
            outcome = run().await;
        }
        enqueue_request_toast(state, &request, outcome, PLAYER_REQUEST_TIMEOUT);
    }
}

/// How long to wait before sending a rejected player command once more:
/// Spotify's `Retry-After`, when it is short enough to sit out.
fn short_rate_limit_wait(outcome: &RequestOutcome) -> Option<Duration> {
    let Ok(Err(err)) = outcome else {
        return None;
    };
    super::rate_limit_retry_after(err).filter(|wait| *wait <= PLAYER_RETRY_MAX_WAIT)
}

/// Whether `queued` is the toggle `sent` pressed again while `sent` waited out
/// a rate limit. The resend delivers what those presses asked for; sending
/// them as well would undo it.
fn repeats_toggle(queued: &ClientRequest, sent: &PlayerRequest) -> bool {
    matches!(
        queued,
        ClientRequest::Player(request)
            if sent.is_toggle()
                && std::mem::discriminant(request) == std::mem::discriminant(sent)
    )
}

/// Toast text for a command Spotify's rate limit rejected.
fn rate_limit_message(action: &str, retry_after: Option<Duration>) -> String {
    match retry_after {
        Some(wait) => format!(
            "{RATE_LIMIT_TOAST_PREFIX}: {action} was not sent. Try again in {} s.",
            wait.as_secs().max(1)
        ),
        None => format!("{RATE_LIMIT_TOAST_PREFIX}: {action} was not sent."),
    }
}

fn enqueue_request_toast(
    state: &SharedState,
    request: &ClientRequest,
    outcome: RequestOutcome,
    timeout: Duration,
) {
    match outcome {
        Ok(Ok(())) => {
            if let Some(message) = request.toast_success_message() {
                state.push_success_toast(message);
            }
        }
        Ok(Err(err)) => {
            tracing::error!("Failed to handle client request: {err:#}");
            // Only what the user asked for; a failed background fetch stays in the log.
            let Some(action) = request.action() else {
                return;
            };
            if super::is_rate_limit(&err) {
                state.push_rate_limit_toast(rate_limit_message(
                    action,
                    super::rate_limit_retry_after(&err),
                ));
            } else {
                state.push_error_toast(format!("Failed: {err:#}"));
            }
        }
        Err(_) => {
            tracing::error!("Timed out after {timeout:?} handling client request");
            if request.action().is_some() {
                state.push_error_toast(format!("Timed out after {timeout:?}"));
            }
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
        (Some(playback), Some(rspotify::model::PlayableItem::Track(track))) => {
            // Local files and ads have no Spotify id; there is nothing to track.
            let Some(id) = track.id.clone() else {
                return Ok(());
            };
            (playback, PlayableId::Track(id), track.duration)
        }
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
            .is_some_and(|queue_track| queue_track.id().is_none_or(|queue_id| queue_id != id)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HttpError;

    fn rejected(status: u16, retry_after: Option<&str>) -> RequestOutcome {
        let mut response = http::Response::builder().status(status);
        if let Some(value) = retry_after {
            response = response.header("retry-after", value);
        }
        let err = rspotify::ClientError::Http(Box::new(HttpError::StatusCode(
            reqwest::Response::from(response.body("").unwrap()),
        )));
        Ok(Err(anyhow::Error::from(err).context("pause playback")))
    }

    #[test]
    fn only_a_short_rate_limit_is_sat_out() {
        assert_eq!(
            short_rate_limit_wait(&rejected(429, Some("4"))),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            short_rate_limit_wait(&rejected(429, Some("5"))),
            Some(PLAYER_RETRY_MAX_WAIT)
        );
        // Most waits Spotify asked for on 2026-10-05 were 26 to 28 s.
        assert_eq!(short_rate_limit_wait(&rejected(429, Some("27"))), None);
        assert_eq!(short_rate_limit_wait(&rejected(429, None)), None);
        assert_eq!(short_rate_limit_wait(&rejected(404, Some("1"))), None);
        assert_eq!(short_rate_limit_wait(&Ok(Ok(()))), None);
    }

    #[test]
    fn presses_of_a_toggle_made_during_the_wait_are_not_sent_again() {
        let queued = |request: PlayerRequest| ClientRequest::Player(request);
        let toggle = PlayerRequest::ResumePause;
        assert!(repeats_toggle(&queued(PlayerRequest::ResumePause), &toggle));
        // Anything else pressed meanwhile still runs.
        assert!(!repeats_toggle(&queued(PlayerRequest::NextTrack), &toggle));
        assert!(!repeats_toggle(&queued(PlayerRequest::Shuffle), &toggle));
        assert!(!repeats_toggle(&ClientRequest::GetCurrentPlayback, &toggle));
        // Two presses of "next" mean two tracks.
        assert!(!repeats_toggle(
            &queued(PlayerRequest::NextTrack),
            &PlayerRequest::NextTrack
        ));
    }

    #[test]
    fn rate_limit_message_says_what_was_lost_and_for_how_long() {
        assert_eq!(
            rate_limit_message("play/pause", Some(Duration::from_secs(27))),
            "Spotify rate limit: play/pause was not sent. Try again in 27 s."
        );
        assert_eq!(
            rate_limit_message("volume", None),
            "Spotify rate limit: volume was not sent."
        );
        assert!(rate_limit_message("seek", None).starts_with(RATE_LIMIT_TOAST_PREFIX));
    }
}
