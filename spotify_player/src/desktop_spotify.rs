//! Launch and MPRIS-nudge the official Spotify desktop client (Linux).
//!
//! Spotify Connect often omits the desktop app from `/v1/me/player/devices`
//! until it has joined a playback session — or lists an idle/paused tray client
//! after autostart that still cannot accept control until nudged. With
//! `enable_streaming = "Never"`, that leaves `spotify_player` unable to transfer
//! until the user hits play in the GUI. This module starts the client if needed
//! and wakes it via MPRIS (`Play` / `OpenUri`) so Connect can use it. That
//! registration nudge runs on first session (and when restoring a playing
//! session); a paused mid-session reconnect skips `OpenUri` so an API blip
//! cannot start music. When Connect has no current playback, it also exposes
//! MPRIS track metadata so the TUI playback window is not empty while the
//! desktop client is already playing.
//! When Connect reports a preferred-device session at silent volume,
//! `overlay_connect_volume` / `mpris_volume_percent` overlay the audible MPRIS
//! volume (the Web API often reports 0% while MPRIS is not).
//!
//! When `pause_after_nudge` is set, that Play/OpenUri is silenced by muting
//! Spotify's PipeWire/Pulse sink-inputs for the wake (Spotify's MPRIS volume
//! is left alone — setting it to 0 can stick the stream at 0% after restore).
//! Mute is held until MPRIS reports paused (retries, then a short background
//! hold). Inputs are unmuted after pause, or after a timeout so mute cannot
//! stick forever. Connect still sees a session; explicit CLI/TUI play starts
//! audible audio.

use std::{
    collections::HashMap,
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rspotify::model::{
    Actions, CurrentPlaybackContext, CurrentlyPlayingType, Device, DeviceType, FullTrack, Image,
    PlayableItem, RepeatState, SimplifiedAlbum, SimplifiedArtist, TrackId, Type,
};

use crate::config::DesktopSpotifyConfig;

const DBUS_DEST_BUS: &str = "org.freedesktop.DBus";
const DBUS_OBJECT: &str = "/org/freedesktop/DBus";
const MPRIS_OBJECT: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
static EARLY_LAUNCH: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeOutcome {
    pub launched: bool,
    pub minimized: bool,
}

/// Whether `ensure_awake` should MPRIS-nudge an already-playing desktop client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgePolicy {
    /// Leave local playback alone when MPRIS reports Playing.
    SkipIfPlaying,
    /// Nudge even while Playing — registers Connect when the GUI is audible but
    /// the preferred device is missing from `/v1/me/player/devices`.
    RegisterConnect,
}

/// Whether waking Spotify will need to start a new desktop process.
pub fn will_launch(config: &DesktopSpotifyConfig) -> Result<bool> {
    Ok(!mpris_name_has_owner(&config.mpris_dest)? && !spotify_process_running())
}

/// Start Spotify immediately, before playback/device Web API initialization.
///
/// Spotify takes several seconds to expose MPRIS and Connect. Starting that work
/// locally lets it overlap authentication and API requests instead of following
/// them serially.
pub fn launch_early_if_needed(config: &DesktopSpotifyConfig) -> Result<bool> {
    if !config.enable {
        return Ok(false);
    }
    if EARLY_LAUNCH.load(Ordering::Acquire) {
        return Ok(true);
    }
    if !will_launch(config)? {
        return Ok(false);
    }

    if config.start_minimized {
        ensure_minimize_to_tray_pref();
        tokio::spawn(async {
            keep_hiding(Duration::from_secs(45), false).await;
        });
    }
    launch(config)?;
    EARLY_LAUNCH.store(true, Ordering::Release);
    tracing::info!("Started Spotify desktop early while playback initializes");
    Ok(true)
}

/// Spotify track URI for the desktop client's current MPRIS session, if any.
pub fn mpris_current_track_uri(dest: &str) -> Option<String> {
    mpris_now_playing(dest)
        .ok()
        .flatten()
        .and_then(|now| now.track_id.map(|id| format!("spotify:track:{id}")))
}

/// Ensure the desktop Spotify client is running and has an active playback
/// session so it appears as a Connect device.
pub async fn ensure_awake(
    config: &DesktopSpotifyConfig,
    nudge_uri: Option<&str>,
    nudge_policy: NudgePolicy,
) -> Result<WakeOutcome> {
    if !config.enable {
        return Ok(WakeOutcome {
            launched: false,
            minimized: false,
        });
    }

    let dest = config.mpris_dest.as_str();
    let launched_early = EARLY_LAUNCH.swap(false, Ordering::AcqRel);
    // Re-check even after an early launch: if that process exited during auth,
    // relaunch here instead of waiting the full MPRIS timeout.
    let needs_launch = will_launch(config)?;
    let launched = launched_early || needs_launch;

    // Spotify's `--minimized` flag is Windows-only. Closing the window too early
    // (before the tray icon is up) can quit the client, so: taskbar-minimize
    // while waiting for MPRIS, then close-to-tray once MPRIS is ready.
    if launched && config.start_minimized {
        ensure_minimize_to_tray_pref();
    }
    let flash_watch = if launched && config.start_minimized {
        Some(tokio::spawn(async {
            keep_hiding(Duration::from_secs(45), false).await;
        }))
    } else {
        None
    };

    if !mpris_name_has_owner(dest)? {
        if needs_launch {
            tracing::info!(
                "Preferred device unavailable; automatically starting Spotify desktop via `{}`",
                config.command
            );
            launch(config)?;
        } else {
            tracing::info!(
                "Desktop Spotify process found but MPRIS not ready; waiting up to {}s",
                config.ready_timeout_secs
            );
        }
        wait_for_mpris(config, Duration::from_secs(config.ready_timeout_secs)).await?;
    }
    if let Some(watch) = flash_watch {
        watch.abort();
    }

    let connect_nudge_uri;
    let nudge_uri = match nudge_policy {
        NudgePolicy::RegisterConnect => {
            connect_nudge_uri =
                mpris_current_track_uri(dest).or_else(|| nudge_uri.map(str::to_owned));
            connect_nudge_uri.as_deref()
        }
        NudgePolicy::SkipIfPlaying => nudge_uri,
    };

    if mpris_is_playing(dest) && nudge_policy == NudgePolicy::SkipIfPlaying {
        tracing::info!(
            "Desktop Spotify is already playing (MPRIS); skipping wake nudge so local playback is left alone"
        );
    } else if mpris_is_playing(dest) && nudge_policy == NudgePolicy::RegisterConnect {
        tracing::info!(
            "Desktop Spotify is playing locally but Connect is missing the preferred device; registering without pausing playback"
        );
        // Do not pause an audible session — OpenUri on the current track is enough
        // for Connect to list the desktop client.
        nudge(dest, nudge_uri, false)?;
    } else {
        nudge(dest, nudge_uri, config.pause_after_nudge)?;
    }
    let minimized = if config.start_minimized {
        // Prefer tray hide once MPRIS (and usually the tray icon) is up.
        // Also run when we only nudged an already-running instance — Connect
        // transfer / OpenUri can map a window that was parked in the tray.
        hide_window().await
    } else {
        false
    };
    Ok(WakeOutcome {
        launched,
        minimized,
    })
}

fn launch(config: &DesktopSpotifyConfig) -> Result<()> {
    let program = resolve_command(&config.command)?;
    Command::new(&program)
        .args(&config.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group: closing the terminal or quitting the player must not
        // take the desktop client down with it.
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to launch desktop Spotify via `{program}`"))?;
    Ok(())
}

fn resolve_command(command: &str) -> Result<String> {
    if Path::new(command).is_absolute() {
        return Ok(command.to_string());
    }
    which::which(command)
        .map(|p| p.to_string_lossy().into_owned())
        .with_context(|| format!("desktop Spotify command `{command}` not found on PATH"))
}

async fn wait_for_mpris(config: &DesktopSpotifyConfig, timeout: Duration) -> Result<()> {
    let dest = config.mpris_dest.as_str();
    let start = Instant::now();
    let poll = Duration::from_millis(400);
    let mut relaunched = false;
    loop {
        let mpris_ready = mpris_name_has_owner(dest)?;
        if mpris_ready {
            tracing::info!("Desktop Spotify MPRIS ready ({dest})");
            return Ok(());
        }
        if should_relaunch_for_mpris(mpris_ready, spotify_process_running(), relaunched) {
            tracing::warn!(
                "Desktop Spotify exited before MPRIS was ready; relaunching `{}`",
                config.command
            );
            launch(config)?;
            relaunched = true;
        }
        if start.elapsed() >= timeout {
            anyhow::bail!("timed out after {timeout:?} waiting for desktop Spotify MPRIS ({dest})");
        }
        tokio::time::sleep(poll).await;
    }
}

fn should_relaunch_for_mpris(
    mpris_ready: bool,
    process_running: bool,
    already_relaunched: bool,
) -> bool {
    !mpris_ready && !process_running && !already_relaunched
}

/// Hide Spotify's UI to the system tray (or taskbar fallback), waiting briefly
/// for a real window to appear.
///
/// With Spotify's `ui.minimize_to_tray` pref, `windowclose` keeps the process
/// alive and parks it in the `StatusNotifier` tray. Callers repeat this after a
/// Connect transfer, which can raise the window again.
pub async fn hide_window() -> bool {
    if which::which("xdotool").is_err() {
        tracing::warn!("`xdotool` is unavailable; cannot hide the Spotify window");
        return false;
    }

    let to_tray = minimize_to_tray_pref_enabled();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found_window = false;
    loop {
        match hide_visible_once(to_tray) {
            Ok(HideVisible::Hidden) => {
                tracing::info!(
                    "Spotify desktop window hidden ({})",
                    if to_tray { "system tray" } else { "taskbar" }
                );
                return true;
            }
            Ok(HideVisible::HidSome) => found_window = true,
            Ok(HideVisible::NoneVisible) if found_window => {
                tracing::info!(
                    "Spotify desktop window hidden ({})",
                    if to_tray { "system tray" } else { "taskbar" }
                );
                return true;
            }
            // Before MPRIS we only taskbar-minimize, never close. Therefore, once
            // tray hiding is allowed, no real UI window means it is already parked
            // in the tray (usually by the post-nudge or post-transfer pass).
            Ok(HideVisible::NoneVisible) if to_tray => {
                tracing::info!("Spotify desktop window already hidden (system tray)");
                return true;
            }
            Ok(HideVisible::NoneVisible) => {}
            Err(err) => {
                tracing::warn!("{err:#}");
                return false;
            }
        }

        if Instant::now() >= deadline {
            // The parallel hide watch (or Spotify itself) may already have parked
            // the UI in the tray before this waiter saw a window.
            if !has_visible_ui_window() {
                tracing::info!(
                    "Spotify desktop window already hidden ({})",
                    if to_tray { "system tray" } else { "taskbar" }
                );
                return true;
            }
            tracing::warn!("No Spotify window found to hide");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll and hide Spotify whenever a real UI window maps.
///
/// `prefer_tray` selects close-to-tray (`windowclose`) vs taskbar minimize.
async fn keep_hiding(max: Duration, prefer_tray: bool) {
    if which::which("xdotool").is_err() {
        return;
    }
    let to_tray = prefer_tray && minimize_to_tray_pref_enabled();
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        let _ = hide_visible_once(to_tray);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HideVisible {
    NoneVisible,
    HidSome,
    Hidden,
}

/// Tray hide must only search mapped visible windows so `windowclose` never
/// targets hidden ghosts (login autostart `wmctrl hidden`, `KWin` no-focus minimize).
fn hide_search_only_visible() -> bool {
    true
}

/// The tray stub window is named `spotify`; closing it tears down the tray entry.
fn is_spotify_main_ui_window_name(name: &str) -> bool {
    !name.trim().eq_ignore_ascii_case("spotify")
}

fn hide_visible_once(to_tray: bool) -> Result<HideVisible> {
    // Only act on mapped, visible main UI windows. Tray hide uses `windowclose`,
    // which must not target hidden/minimized ghosts (e.g. from login autostart or
    // a KWin no-focus rule): closing those leaves Spotify thinking the UI is
    // shown while nothing is actually visible ("Show Spotify" toggles to
    // "Minimize to Tray" without mapping a window).
    let ids = ui_window_ids(hide_search_only_visible())?;
    if ids.is_empty() {
        return Ok(HideVisible::NoneVisible);
    }

    let action = if to_tray {
        "windowclose"
    } else {
        "windowminimize"
    };
    for id in &ids {
        let status = Command::new("xdotool")
            .args([action, id.as_str()])
            .status()
            .with_context(|| format!("`xdotool {action}` failed"))?;
        if !status.success() {
            anyhow::bail!("Hiding the Spotify window via {action} failed with {status}");
        }
    }

    if has_visible_ui_window() {
        Ok(HideVisible::HidSome)
    } else {
        Ok(HideVisible::Hidden)
    }
}

fn has_visible_ui_window() -> bool {
    ui_window_ids(true).is_ok_and(|ids| !ids.is_empty())
}

fn ui_window_ids(only_visible: bool) -> Result<Vec<String>> {
    let mut command = Command::new("xdotool");
    command.arg("search");
    if only_visible {
        command.arg("--onlyvisible");
    }
    let output = command
        .args(["--class", "spotify"])
        .output()
        .context("`xdotool` search failed")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // The stub window left behind for the tray icon is named "spotify";
        // closing it can tear the tray entry down. Only hide real UI windows.
        .filter(|id| {
            Command::new("xdotool")
                .args(["getwindowname", id])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .is_some_and(|name| is_spotify_main_ui_window_name(&name))
        })
        .map(str::to_owned)
        .collect())
}

/// Best-effort: set Spotify's "Minimize Spotify to the tray" pref before launch.
///
/// Without this, `windowclose` can quit the client instead of parking it in the tray.
fn ensure_minimize_to_tray_pref() {
    for path in spotify_user_prefs_paths() {
        match set_prefs_bool(&path, "ui.minimize_to_tray", true) {
            Ok(true) => tracing::info!(
                "Enabled Spotify tray hide (`ui.minimize_to_tray=true`) in {}",
                path.display()
            ),
            Ok(false) => {}
            Err(err) => tracing::warn!(
                "Could not enable Spotify tray hide in {}: {err:#}",
                path.display()
            ),
        }
    }
}

fn minimize_to_tray_pref_enabled() -> bool {
    spotify_user_prefs_paths().into_iter().any(|path| {
        fs::read_to_string(&path).ok().is_some_and(|contents| {
            contents
                .lines()
                .any(|line| line.trim() == "ui.minimize_to_tray=true")
        })
    })
}

fn spotify_user_prefs_paths() -> Vec<PathBuf> {
    let Some(home) = dirs_next::home_dir() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for root in [
        home.join("snap/spotify/current/.config/spotify/Users"),
        home.join(".config/spotify/Users"),
    ] {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let prefs = entry.path().join("prefs");
            if prefs.is_file() {
                paths.push(prefs);
            }
        }
    }
    paths
}

fn set_prefs_bool(path: &Path, key: &str, value: bool) -> Result<bool> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let wanted = format!("{key}={}", if value { "true" } else { "false" });
    let mut changed = false;
    let mut found = false;
    let mut lines: Vec<String> = contents
        .lines()
        .map(|line| {
            if let Some((k, _)) = line.split_once('=') {
                if k.trim() == key {
                    found = true;
                    if line.trim() != wanted {
                        changed = true;
                        return wanted.clone();
                    }
                }
            }
            line.to_string()
        })
        .collect();
    if !found {
        lines.push(wanted);
        changed = true;
    }
    if changed {
        let mut out = lines.join("\n");
        if !out.ends_with('\n') {
            out.push('\n');
        }
        fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(changed)
}

fn nudge(dest: &str, nudge_uri: Option<&str>, pause_after: bool) -> Result<()> {
    let uri = nudge_uri
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_spotify_uri);

    // Connect only lists the desktop client after a real playback session.
    // Mute locally for the duration of that session when we intend to pause
    // immediately afterward, so the user does not hear the registration Play.
    let pulse_mute = pause_after.then(PulseMuteGuard::start);
    if pause_after {
        let _ = mpris_call(dest, "Pause");
    }

    if let Some(uri) = uri {
        tracing::info!("Nudging desktop Spotify via OpenUri ({uri})");
        mpris_open_uri(dest, &uri)?;
    } else {
        tracing::info!("Nudging desktop Spotify via Play (no nudge URI)");
        mpris_call(dest, "Play")?;
    }

    if pause_after {
        // Pause immediately; Connect still sees the brief OpenUri session.
        tracing::info!("Pausing desktop Spotify after silent wake nudge");
        if pause_until_silent(dest, Duration::from_secs(2)) {
            tracing::info!("Desktop Spotify paused; unmuting local sink-inputs");
        } else {
            tracing::warn!(
                "Desktop Spotify did not pause after silent wake; holding mute until pause confirms"
            );
            hold_silence_until_paused(dest.to_string(), pulse_mute);
            return Ok(());
        }
    }

    Ok(())
}

/// Convert `https://open.spotify.com/{type}/{id}` into `spotify:{type}:{id}`.
pub fn normalize_spotify_uri(uri: &str) -> String {
    let uri = uri.trim();
    if let Some(rest) = uri.strip_prefix("https://open.spotify.com/") {
        let rest = rest.split('?').next().unwrap_or(rest);
        let mut parts = rest.split('/');
        if let (Some(kind), Some(id)) = (parts.next(), parts.next()) {
            if !kind.is_empty() && !id.is_empty() {
                return format!("spotify:{kind}:{id}");
            }
        }
    }
    uri.to_string()
}

fn mpris_name_has_owner(dest: &str) -> Result<bool> {
    let output = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply=literal",
            &format!("--dest={DBUS_DEST_BUS}"),
            DBUS_OBJECT,
            "org.freedesktop.DBus.NameHasOwner",
            &format!("string:{dest}"),
        ])
        .output()
        .context("failed to run dbus-send (is D-Bus available?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("dbus-send NameHasOwner failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.contains("boolean true") || stdout.contains("true"))
}

fn mpris_call(dest: &str, method: &str) -> Result<()> {
    let status = Command::new("dbus-send")
        .args([
            "--session",
            "--type=method_call",
            &format!("--dest={dest}"),
            MPRIS_OBJECT,
            &format!("{MPRIS_PLAYER}.{method}"),
        ])
        .status()
        .with_context(|| format!("failed to call MPRIS {method}"))?;

    if !status.success() {
        anyhow::bail!("MPRIS {method} failed with status {status}");
    }
    Ok(())
}

fn mpris_open_uri(dest: &str, uri: &str) -> Result<()> {
    let status = Command::new("dbus-send")
        .args([
            "--session",
            "--type=method_call",
            &format!("--dest={dest}"),
            MPRIS_OBJECT,
            &format!("{MPRIS_PLAYER}.OpenUri"),
            &format!("string:{uri}"),
        ])
        .status()
        .context("failed to call MPRIS OpenUri")?;

    if !status.success() {
        anyhow::bail!("MPRIS OpenUri failed with status {status}");
    }
    Ok(())
}

const DBUS_PROPERTIES: &str = "org.freedesktop.DBus.Properties";

fn mpris_get_playback_status(dest: &str) -> Result<String> {
    let output = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply=literal",
            "--type=method_call",
            &format!("--dest={dest}"),
            MPRIS_OBJECT,
            &format!("{DBUS_PROPERTIES}.Get"),
            &format!("string:{MPRIS_PLAYER}"),
            "string:PlaybackStatus",
        ])
        .output()
        .context("failed to read MPRIS PlaybackStatus")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("MPRIS Get PlaybackStatus failed: {stderr}");
    }

    parse_mpris_playback_status_reply(&String::from_utf8_lossy(&output.stdout))
        .map(str::to_string)
        .context("could not parse MPRIS PlaybackStatus")
}

fn mpris_is_silent(dest: &str) -> bool {
    mpris_get_playback_status(dest)
        .ok()
        .as_deref()
        .is_some_and(playback_status_is_silent)
}

/// Whether the official desktop client is currently playing via MPRIS.
///
/// Connect often omits that client from `/v1/me/player` even while the GUI is
/// already playing. Callers must treat this as local active playback and must
/// not OpenUri/Pause it as a "wake".
pub fn mpris_is_playing(dest: &str) -> bool {
    mpris_get_playback_status(dest)
        .ok()
        .as_deref()
        .is_some_and(playback_status_is_playing)
}

fn playback_status_is_playing(status: &str) -> bool {
    status.eq_ignore_ascii_case("Playing")
}

/// Local desktop-client playback parsed from MPRIS (Connect may still be empty).
#[derive(Debug, Clone, PartialEq)]
struct MprisNowPlaying {
    title: String,
    artists: Vec<String>,
    album: String,
    length: chrono::Duration,
    position: chrono::Duration,
    is_playing: bool,
    track_id: Option<String>,
    art_url: Option<String>,
    track_number: u32,
    volume_percent: Option<u32>,
    shuffle: bool,
    repeat: RepeatState,
}

/// Connect `/v1/me/player` often returns null while the official client is
/// already Playing (or paused on a loaded track) via MPRIS. Use that metadata
/// for the playback window until Connect lists a session.
pub fn current_playback_from_mpris(
    dest: &str,
    device_name: &str,
) -> Result<Option<CurrentPlaybackContext>> {
    Ok(mpris_now_playing(dest)?.map(|now| playback_context_from_mpris(now, device_name)))
}

fn mpris_now_playing(dest: &str) -> Result<Option<MprisNowPlaying>> {
    let status = mpris_get_playback_status(dest)?;
    if status.eq_ignore_ascii_case("Stopped") {
        return Ok(None);
    }
    let metadata_reply = mpris_get_property_reply(dest, "Metadata")?;
    let parsed = parse_mpris_metadata_reply(&metadata_reply);
    let position_us = mpris_get_property_reply(dest, "Position")
        .ok()
        .as_deref()
        .and_then(parse_int_after_token_any)
        .unwrap_or(0);
    let volume_percent = mpris_get_property_reply(dest, "Volume")
        .ok()
        .as_deref()
        .and_then(parse_volume_percent);
    let shuffle = mpris_get_property_reply(dest, "Shuffle")
        .ok()
        .as_deref()
        .is_some_and(|s| s.contains("true"));
    let repeat = mpris_get_property_reply(dest, "LoopStatus")
        .ok()
        .as_deref()
        .map_or(RepeatState::Off, parse_loop_status);
    Ok(now_playing_from_parsed(
        &status,
        parsed,
        position_us,
        volume_percent,
        shuffle,
        repeat,
    ))
}

fn now_playing_from_parsed(
    status: &str,
    parsed: ParsedMprisMetadata,
    position_us: i64,
    volume_percent: Option<u32>,
    shuffle: bool,
    repeat: RepeatState,
) -> Option<MprisNowPlaying> {
    if parsed.title.is_empty() {
        return None;
    }
    Some(MprisNowPlaying {
        title: parsed.title,
        artists: parsed.artists,
        album: parsed.album,
        length: chrono::Duration::microseconds(parsed.length_us.max(0)),
        position: chrono::Duration::microseconds(position_us.max(0)),
        is_playing: playback_status_is_playing(status),
        track_id: parsed.track_id,
        art_url: parsed.art_url,
        track_number: parsed.track_number.unwrap_or(1),
        volume_percent,
        shuffle,
        repeat,
    })
}

fn playback_context_from_mpris(now: MprisNowPlaying, device_name: &str) -> CurrentPlaybackContext {
    let artists = if now.artists.is_empty() {
        vec![simplified_artist("Unknown")]
    } else {
        now.artists
            .iter()
            .map(|name| simplified_artist(name))
            .collect()
    };
    let images = now
        .art_url
        .as_ref()
        .map(|url| Image {
            url: url.clone(),
            height: None,
            width: None,
        })
        .into_iter()
        .collect();
    let track_id = now
        .track_id
        .as_deref()
        .and_then(|id| TrackId::from_id(id).ok().map(TrackId::into_static));
    #[allow(deprecated)]
    let track = FullTrack {
        album: SimplifiedAlbum {
            album_type: None,
            artists: artists.clone(),
            external_urls: HashMap::new(),
            href: None,
            id: None,
            images,
            name: now.album,
            release_date: None,
            release_date_precision: None,
            restrictions: None,
            ..Default::default()
        },
        artists,
        available_markets: Vec::new(),
        disc_number: 1,
        duration: now.length,
        explicit: false,
        external_ids: HashMap::new(),
        external_urls: HashMap::new(),
        href: None,
        id: track_id,
        is_local: false,
        is_playable: Some(true),
        linked_from: None,
        restrictions: None,
        name: now.title,
        popularity: 0,
        preview_url: None,
        track_number: now.track_number,
        r#type: Type::Track,
    };
    CurrentPlaybackContext {
        device: Device {
            id: None,
            is_active: true,
            is_private_session: false,
            is_restricted: false,
            name: device_name.to_string(),
            _type: DeviceType::Computer,
            volume_percent: now.volume_percent,
        },
        repeat_state: now.repeat,
        shuffle_state: now.shuffle,
        context: None,
        timestamp: chrono::Utc::now(),
        progress: Some(now.position),
        is_playing: now.is_playing,
        item: Some(PlayableItem::Track(track)),
        currently_playing_type: CurrentlyPlayingType::Track,
        actions: Actions::default(),
    }
}

fn simplified_artist(name: &str) -> SimplifiedArtist {
    SimplifiedArtist {
        external_urls: HashMap::new(),
        href: None,
        id: None,
        name: name.to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedMprisMetadata {
    title: String,
    artists: Vec<String>,
    album: String,
    length_us: i64,
    track_id: Option<String>,
    art_url: Option<String>,
    track_number: Option<u32>,
}

fn parse_mpris_metadata_reply(stdout: &str) -> ParsedMprisMetadata {
    let mut parsed = ParsedMprisMetadata::default();
    for (key, value) in metadata_dict_entries(stdout) {
        match key.as_str() {
            "xesam:title" => {
                parsed.title = first_quoted(&value).unwrap_or_default().to_string();
            }
            "xesam:album" => {
                parsed.album = first_quoted(&value).unwrap_or_default().to_string();
            }
            "xesam:artist" => {
                parsed.artists = quoted_strings(&value);
            }
            "mpris:length" => {
                parsed.length_us = parse_int_after_token_any(&value).unwrap_or(0);
            }
            "mpris:trackid" | "mpris:trackId" => {
                parsed.track_id = first_quoted(&value).and_then(spotify_track_id_from_mpris_text);
            }
            "xesam:url" => {
                if parsed.track_id.is_none() {
                    parsed.track_id =
                        first_quoted(&value).and_then(spotify_track_id_from_mpris_text);
                }
            }
            "mpris:artUrl" => {
                parsed.art_url = first_quoted(&value).map(str::to_string);
            }
            "xesam:trackNumber" => {
                parsed.track_number = parse_int_after_token_any(&value).map(|n| n as u32);
            }
            _ => {}
        }
    }
    parsed
}

fn metadata_dict_entries(stdout: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut rest = stdout;
    while let Some(idx) = rest.find("dict entry(") {
        rest = &rest[idx + "dict entry(".len()..];
        let (this, next) = match rest.find("dict entry(") {
            Some(n) => rest.split_at(n),
            None => (rest, ""),
        };
        if let Some(key) = first_quoted(this) {
            let after_key = this
                .split_once(&format!("\"{key}\""))
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_default();
            entries.push((key.to_string(), after_key));
        }
        rest = next;
    }
    entries
}

fn first_quoted(s: &str) -> Option<&str> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn quoted_strings(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('"') {
        rest = &rest[start + 1..];
        match rest.find('"') {
            Some(end) => {
                out.push(rest[..end].to_string());
                rest = &rest[end + 1..];
            }
            None => break,
        }
    }
    out
}

fn parse_int_after_token_any(s: &str) -> Option<i64> {
    for token in ["int64", "uint64", "int32"] {
        if let Some(idx) = s.find(token) {
            let num = s[idx + token.len()..].split_whitespace().next()?;
            return num.parse().ok();
        }
    }
    None
}

/// Prefer a non-zero MPRIS percent when Connect reports 0 or missing volume.
pub(crate) fn overlay_connect_volume(
    connect_volume: Option<u32>,
    mpris_volume: Option<u32>,
) -> Option<u32> {
    match connect_volume {
        Some(v) if v > 0 => Some(v),
        _ => mpris_volume.filter(|&v| v > 0).or(connect_volume),
    }
}

/// Desktop client's MPRIS Volume as a percent (0–100).
pub(crate) fn mpris_volume_percent(dest: &str) -> Option<u32> {
    mpris_get_property_reply(dest, "Volume")
        .ok()
        .as_deref()
        .and_then(parse_volume_percent)
}

fn parse_volume_percent(s: &str) -> Option<u32> {
    let idx = s.find("double")?;
    let num: f64 = s[idx + "double".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((num.clamp(0.0, 1.0) * 100.0).round() as u32)
}

fn parse_loop_status(s: &str) -> RepeatState {
    let text = s.to_ascii_lowercase();
    if text.contains("track") {
        RepeatState::Track
    } else if text.contains("playlist") {
        RepeatState::Context
    } else {
        RepeatState::Off
    }
}

fn spotify_track_id_from_mpris_text(text: &str) -> Option<String> {
    const ID_LEN: usize = 22;
    if let Some(idx) = text.rfind("/track/") {
        let id = text[idx + "/track/".len()..]
            .split(['?', '/'])
            .next()
            .unwrap_or("");
        if id.len() == ID_LEN {
            return Some(id.to_string());
        }
    }
    if let Some((_, rest)) = text.split_once("track:") {
        let id = rest.split(['?', '/']).next().unwrap_or("");
        if id.len() == ID_LEN {
            return Some(id.to_string());
        }
    }
    if text.len() == ID_LEN && text.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(text.to_string());
    }
    None
}

fn mpris_get_property_reply(dest: &str, name: &str) -> Result<String> {
    let output = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--type=method_call",
            &format!("--dest={dest}"),
            MPRIS_OBJECT,
            &format!("{DBUS_PROPERTIES}.Get"),
            &format!("string:{MPRIS_PLAYER}"),
            &format!("string:{name}"),
        ])
        .output()
        .with_context(|| format!("failed to read MPRIS {name}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("MPRIS Get {name} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Retry Pause until MPRIS reports Paused/Stopped, or `budget` elapses.
fn pause_until_silent(dest: &str, budget: Duration) -> bool {
    let start = Instant::now();
    loop {
        let _ = mpris_call(dest, "Pause");
        if mpris_is_silent(dest) {
            return true;
        }
        if pause_poll_should_stop(false, start.elapsed(), budget) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn hold_silence_until_paused(dest: String, pulse_mute: Option<PulseMuteGuard>) {
    thread::spawn(move || {
        let budget = Duration::from_secs(10);
        if pause_until_silent(&dest, budget) {
            tracing::info!("Desktop Spotify paused after delayed silent-wake hold");
        } else {
            tracing::warn!(
                "Desktop Spotify still playing after {budget:?}; restoring audio to avoid a stuck mute"
            );
        }
        drop(pulse_mute);
    });
}

/// Stop polling once playback is silent, or the retry budget is spent.
fn pause_poll_should_stop(confirmed_silent: bool, elapsed: Duration, budget: Duration) -> bool {
    confirmed_silent || elapsed >= budget
}

fn playback_status_is_silent(status: &str) -> bool {
    status.eq_ignore_ascii_case("Paused") || status.eq_ignore_ascii_case("Stopped")
}

/// Parse `dbus-send --print-reply=literal` `PlaybackStatus` (`Playing`/`Paused`/`Stopped`).
fn parse_mpris_playback_status_reply(stdout: &str) -> Option<&'static str> {
    let text = stdout.to_ascii_lowercase();
    if text.contains("paused") {
        Some("Paused")
    } else if text.contains("stopped") {
        Some("Stopped")
    } else if text.contains("playing") {
        Some("Playing")
    } else {
        None
    }
}

fn is_spotify_client_binary(binary: &str) -> bool {
    Path::new(binary)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "spotify")
}

fn is_spotify_sink_input(input: &serde_json::Value) -> bool {
    let props = input.get("properties");
    let binary = props
        .and_then(|p| p.get("application.process.binary"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if is_spotify_client_binary(binary) {
        return true;
    }
    // Snap Spotify often omits process.binary; match the stream name instead.
    props
        .and_then(|p| p.get("application.name"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("spotify"))
}

fn sink_input_volume_is_silent(input: &serde_json::Value) -> bool {
    let Some(volume) = input.get("volume").and_then(serde_json::Value::as_object) else {
        return false;
    };
    !volume.is_empty()
        && volume.values().all(|ch| {
            ch.get("value")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|v| v == 0)
        })
}

#[cfg(test)]
fn spotify_sink_input_indices_from_json(json: &str) -> Vec<u32> {
    spotify_sink_inputs_from_json(json)
        .into_iter()
        .map(|(index, _)| index)
        .collect()
}

fn spotify_sink_inputs_from_json(json: &str) -> Vec<(u32, bool)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(inputs) = value.as_array() else {
        return Vec::new();
    };
    inputs
        .iter()
        .filter(|input| is_spotify_sink_input(input))
        .filter_map(|input| {
            let index = input.get("index")?.as_u64()? as u32;
            Some((index, sink_input_volume_is_silent(input)))
        })
        .collect()
}

fn indices_to_restore(recorded: &[u32], current: &[(u32, bool)]) -> Vec<(u32, bool)> {
    let mut out: Vec<(u32, bool)> = current.to_vec();
    for index in recorded {
        if !out.iter().any(|(i, _)| i == index) {
            out.push((*index, false));
        }
    }
    out
}

fn spotify_sink_inputs() -> Vec<(u32, bool)> {
    let output = Command::new("pactl")
        .args(["--format=json", "list", "sink-inputs"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    spotify_sink_inputs_from_json(&String::from_utf8_lossy(&output.stdout))
}

fn set_sink_input_mute(index: u32, mute: bool) -> bool {
    Command::new("pactl")
        .args([
            "set-sink-input-mute",
            &index.to_string(),
            if mute { "1" } else { "0" },
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn set_sink_input_volume_100(index: u32) -> bool {
    Command::new("pactl")
        .args(["set-sink-input-volume", &index.to_string(), "100%"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn restore_spotify_sink_inputs(recorded: &[u32]) {
    for (index, silent_volume) in indices_to_restore(recorded, &spotify_sink_inputs()) {
        let _ = set_sink_input_mute(index, false);
        if silent_volume {
            let _ = set_sink_input_volume_100(index);
        }
    }
}

/// Mute Spotify's local audio streams for the wake Play, then unmute on drop.
struct PulseMuteGuard {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<Vec<u32>>>,
}

impl PulseMuteGuard {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let mut muted = Vec::new();
            while !stop_thread.load(Ordering::Relaxed) {
                for (index, _) in spotify_sink_inputs() {
                    if !muted.contains(&index) && set_sink_input_mute(index, true) {
                        muted.push(index);
                    }
                }
                thread::sleep(Duration::from_millis(40));
            }
            muted
        });
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for PulseMuteGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let recorded = self
            .join
            .take()
            .and_then(|join| join.join().ok())
            .unwrap_or_default();
        restore_spotify_sink_inputs(&recorded);
    }
}

fn spotify_process_running() -> bool {
    // Match the official client binary name without catching spotify_player / spotifyd.
    Command::new("pgrep")
        .args(["-x", "spotify"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Resolve a nudge URI from config or a recently-played track id.
pub fn resolve_nudge_uri(
    config_uri: Option<&str>,
    recent_track_uri: Option<&str>,
) -> Option<String> {
    config_uri
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_spotify_uri)
        .or_else(|| {
            recent_track_uri
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(normalize_spotify_uri)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_open_spotify_https_urls() {
        assert_eq!(
            normalize_spotify_uri("https://open.spotify.com/track/48GaWKv1HXhXJ3cDrgqZ4W?si=abc"),
            "spotify:track:48GaWKv1HXhXJ3cDrgqZ4W"
        );
        assert_eq!(
            normalize_spotify_uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"
        );
    }

    #[test]
    fn resolve_prefers_config_over_recent() {
        assert_eq!(
            resolve_nudge_uri(
                Some("https://open.spotify.com/track/aaa"),
                Some("spotify:track:bbb")
            )
            .as_deref(),
            Some("spotify:track:aaa")
        );
        assert_eq!(
            resolve_nudge_uri(None, Some("spotify:track:bbb")).as_deref(),
            Some("spotify:track:bbb")
        );
        assert_eq!(resolve_nudge_uri(Some("  "), None), None);
    }

    #[test]
    fn name_has_owner_false_is_not_error() {
        // Smoke: dbus-send exists in CI/dev Linux; don't require Spotify running.
        let result =
            mpris_name_has_owner("org.mpris.MediaPlayer2.spotify-player-wake-test-missing");
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn hide_search_only_visible_is_always_true() {
        assert!(hide_search_only_visible());
    }

    #[test]
    fn spotify_main_ui_window_name_excludes_tray_stub() {
        assert!(!is_spotify_main_ui_window_name("spotify"));
        assert!(!is_spotify_main_ui_window_name(" Spotify "));
        assert!(is_spotify_main_ui_window_name("Spotify Premium"));
        assert!(is_spotify_main_ui_window_name("Artist - Track"));
    }

    #[test]
    fn relaunches_once_when_process_is_gone_before_mpris_is_ready() {
        assert!(should_relaunch_for_mpris(false, false, false));
        assert!(!should_relaunch_for_mpris(false, false, true));
        assert!(!should_relaunch_for_mpris(false, true, false));
        assert!(!should_relaunch_for_mpris(true, false, false));
    }

    #[test]
    fn extracts_spotify_sink_input_indices_from_pactl_json() {
        let json = r#"
        [
          {"index": 12, "properties": {"application.process.binary": "spotify"}},
          {"index": 13, "properties": {"application.process.binary": "spotify_player"}},
          {"index": 14, "properties": {"application.process.binary": "/usr/bin/spotify"}},
          {"index": 15, "properties": {"application.name": "Spotify"}}
        ]
        "#;
        assert_eq!(spotify_sink_input_indices_from_json(json), vec![12, 14, 15]);
        assert!(spotify_sink_input_indices_from_json("not json").is_empty());
    }

    #[test]
    fn detects_silent_sink_input_volume() {
        let json = r#"
        [
          {"index": 1, "properties": {"application.name": "Spotify"}, "volume": {"aux0": {"value": 0}, "aux1": {"value": 0}}},
          {"index": 2, "properties": {"application.name": "Spotify"}, "volume": {"aux0": {"value": 65536}}}
        ]
        "#;
        assert_eq!(
            spotify_sink_inputs_from_json(json),
            vec![(1, true), (2, false)]
        );
    }

    #[test]
    fn restore_includes_current_and_recorded_indices() {
        let current = vec![(99, true)];
        assert_eq!(
            indices_to_restore(&[12, 99], &current),
            vec![(99, true), (12, false)]
        );
    }

    #[test]
    fn parses_literal_mpris_playback_status() {
        assert_eq!(
            parse_mpris_playback_status_reply("   variant       string \"Paused\"\n"),
            Some("Paused")
        );
        assert_eq!(
            parse_mpris_playback_status_reply("string Playing"),
            Some("Playing")
        );
        assert_eq!(
            parse_mpris_playback_status_reply("string Stopped"),
            Some("Stopped")
        );
        assert_eq!(parse_mpris_playback_status_reply("garbage"), None);
        assert!(playback_status_is_silent("Paused"));
        assert!(playback_status_is_silent("stopped"));
        assert!(!playback_status_is_silent("Playing"));
        assert!(playback_status_is_playing("Playing"));
        assert!(playback_status_is_playing("playing"));
        assert!(!playback_status_is_playing("Paused"));
    }

    const SAMPLE_METADATA: &str = r#"
method return
   variant       array [
         dict entry(
            string "mpris:trackid"
            variant                string "/com/spotify/track/6lmsHxA47XsTQ1BPL1PMx7"
         )
         dict entry(
            string "mpris:length"
            variant                uint64 152000000
         )
         dict entry(
            string "mpris:artUrl"
            variant                string "https://i.scdn.co/image/ab67616d0000b2735e968be90e158a68975426b8"
         )
         dict entry(
            string "xesam:album"
            variant                string "Paradise Records (Compilation)"
         )
         dict entry(
            string "xesam:artist"
            variant                array [
                  string "Logic"
               ]
         )
         dict entry(
            string "xesam:title"
            variant                string "Raider of the Lost Art"
         )
         dict entry(
            string "xesam:trackNumber"
            variant                int32 3
         )
         dict entry(
            string "xesam:url"
            variant                string "https://open.spotify.com/track/6lmsHxA47XsTQ1BPL1PMx7"
         )
      ]
"#;

    #[test]
    fn parses_mpris_metadata_dict_for_playback_window() {
        let parsed = parse_mpris_metadata_reply(SAMPLE_METADATA);
        assert_eq!(parsed.title, "Raider of the Lost Art");
        assert_eq!(parsed.artists, vec!["Logic"]);
        assert_eq!(parsed.album, "Paradise Records (Compilation)");
        assert_eq!(parsed.length_us, 152_000_000);
        assert_eq!(parsed.track_id.as_deref(), Some("6lmsHxA47XsTQ1BPL1PMx7"));
        assert_eq!(
            parsed.art_url.as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b2735e968be90e158a68975426b8")
        );
        assert_eq!(parsed.track_number, Some(3));
    }

    #[test]
    fn mpris_now_playing_skips_empty_title_and_stopped() {
        let parsed = ParsedMprisMetadata::default();
        assert!(
            now_playing_from_parsed("Playing", parsed, 0, None, false, RepeatState::Off).is_none()
        );
    }

    #[test]
    fn mpris_fallback_builds_connect_shaped_playback() {
        let parsed = parse_mpris_metadata_reply(SAMPLE_METADATA);
        let now = now_playing_from_parsed(
            "Playing",
            parsed,
            47_166_000,
            Some(80),
            false,
            RepeatState::Off,
        )
        .expect("title present");
        let playback = playback_context_from_mpris(now, "estelle");
        assert!(playback.is_playing);
        assert_eq!(playback.device.name, "estelle");
        assert_eq!(playback.device.id, None);
        assert_eq!(playback.device.volume_percent, Some(80));
        assert_eq!(
            playback.progress,
            Some(chrono::Duration::microseconds(47_166_000))
        );
        match playback.item {
            Some(PlayableItem::Track(track)) => {
                assert_eq!(track.name, "Raider of the Lost Art");
                assert_eq!(track.artists[0].name, "Logic");
                assert_eq!(track.album.name, "Paradise Records (Compilation)");
                assert_eq!(track.duration, chrono::Duration::microseconds(152_000_000));
                assert_eq!(
                    track.id.as_ref().map(rspotify::prelude::Id::id),
                    Some("6lmsHxA47XsTQ1BPL1PMx7")
                );
                assert_eq!(
                    track.album.images[0].url,
                    "https://i.scdn.co/image/ab67616d0000b2735e968be90e158a68975426b8"
                );
            }
            other => panic!("expected track, got {other:?}"),
        }
    }

    #[test]
    fn parses_spotify_ids_from_mpris_trackid_and_url() {
        assert_eq!(
            spotify_track_id_from_mpris_text("/com/spotify/track/6lmsHxA47XsTQ1BPL1PMx7")
                .as_deref(),
            Some("6lmsHxA47XsTQ1BPL1PMx7")
        );
        assert_eq!(
            spotify_track_id_from_mpris_text(
                "https://open.spotify.com/track/6lmsHxA47XsTQ1BPL1PMx7"
            )
            .as_deref(),
            Some("6lmsHxA47XsTQ1BPL1PMx7")
        );
        assert_eq!(
            spotify_track_id_from_mpris_text("spotify:track:6lmsHxA47XsTQ1BPL1PMx7").as_deref(),
            Some("6lmsHxA47XsTQ1BPL1PMx7")
        );
    }

    #[test]
    fn overlay_connect_volume_prefers_audible_mpris_when_connect_is_silent() {
        assert_eq!(overlay_connect_volume(Some(0), Some(100)), Some(100));
        assert_eq!(overlay_connect_volume(None, Some(80)), Some(80));
        assert_eq!(overlay_connect_volume(Some(70), Some(100)), Some(70));
        assert_eq!(overlay_connect_volume(Some(0), Some(0)), Some(0));
        assert_eq!(overlay_connect_volume(Some(0), None), Some(0));
    }

    #[test]
    fn parses_dbus_send_double_volume() {
        assert_eq!(
            parse_volume_percent("method return\n   variant       double 1\n"),
            Some(100)
        );
        assert_eq!(parse_volume_percent("variant double 0.8"), Some(80));
        assert_eq!(parse_volume_percent("variant double 0"), Some(0));
        assert_eq!(parse_volume_percent("no volume here"), None);
    }

    #[test]
    fn pause_poll_holds_mute_until_silent_or_budget() {
        assert!(!pause_poll_should_stop(
            false,
            Duration::from_millis(100),
            Duration::from_secs(2)
        ));
        assert!(pause_poll_should_stop(
            true,
            Duration::from_millis(100),
            Duration::from_secs(2)
        ));
        assert!(pause_poll_should_stop(
            false,
            Duration::from_secs(2),
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn identifies_official_spotify_binary_only() {
        assert!(is_spotify_client_binary("spotify"));
        assert!(is_spotify_client_binary("/usr/bin/spotify"));
        assert!(!is_spotify_client_binary("spotify_player"));
        assert!(!is_spotify_client_binary("/usr/local/bin/spotify_player"));
        assert!(!is_spotify_client_binary("spotifyd"));
    }
}
