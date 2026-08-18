//! Launch and MPRIS-nudge the official Spotify desktop client (Linux).
//!
//! Spotify Connect often omits the desktop app from `/v1/me/player/devices`
//! until it has joined a playback session — or lists an idle/paused tray client
//! after autostart that still cannot accept control until nudged. With
//! `enable_streaming = "Never"`, that leaves `spotify_player` unable to transfer
//! until the user hits play in the GUI. This module starts the client if needed
//! and wakes it via MPRIS (`Play` / `OpenUri`) so Connect can use it.
//!
//! When `pause_after_nudge` is set, that Play/OpenUri is silenced: MPRIS volume
//! is set to 0 and any Spotify PipeWire/Pulse sink-input is muted for the wake.
//! Mute is held until MPRIS reports paused (retries, then a short background
//! hold). Volume/mute are restored after pause, or after a timeout so mute
//! cannot stick forever. Connect still sees a session; explicit CLI/TUI play
//! starts audible audio.

use std::{
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

/// Ensure the desktop Spotify client is running and has an active playback
/// session so it appears as a Connect device.
pub async fn ensure_awake(
    config: &DesktopSpotifyConfig,
    nudge_uri: Option<&str>,
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

    nudge(dest, nudge_uri, config.pause_after_nudge)?;
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

fn hide_visible_once(to_tray: bool) -> Result<HideVisible> {
    // Only act on mapped, visible main UI windows. Tray hide uses `windowclose`,
    // which must not target hidden/minimized ghosts (e.g. from login autostart or
    // a KWin no-focus rule): closing those leaves Spotify thinking the UI is
    // shown while nothing is actually visible ("Show Spotify" toggles to
    // "Minimize to Tray" without mapping a window).
    let ids = ui_window_ids(true)?;
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
                .is_some_and(|name| !name.trim().eq_ignore_ascii_case("spotify"))
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
    let volume_guard = pause_after.then(|| MprisVolumeGuard::start(dest)).flatten();
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
        // Give Connect a moment to register the device before pausing.
        std::thread::sleep(Duration::from_millis(800));
        tracing::info!("Pausing desktop Spotify after silent wake nudge");
        if pause_until_silent(dest, Duration::from_secs(2)) {
            tracing::info!("Desktop Spotify paused; restoring local volume");
        } else {
            tracing::warn!(
                "Desktop Spotify did not pause after silent wake; holding mute until pause confirms"
            );
            hold_silence_until_paused(dest.to_string(), pulse_mute, volume_guard);
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

fn mpris_get_volume(dest: &str) -> Result<f64> {
    let output = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply=literal",
            "--type=method_call",
            &format!("--dest={dest}"),
            MPRIS_OBJECT,
            &format!("{DBUS_PROPERTIES}.Get"),
            &format!("string:{MPRIS_PLAYER}"),
            "string:Volume",
        ])
        .output()
        .context("failed to read MPRIS Volume")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("MPRIS Get Volume failed: {stderr}");
    }

    parse_mpris_volume_reply(&String::from_utf8_lossy(&output.stdout))
        .context("could not parse MPRIS Volume")
}

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

fn hold_silence_until_paused(
    dest: String,
    pulse_mute: Option<PulseMuteGuard>,
    volume_guard: Option<MprisVolumeGuard>,
) {
    thread::spawn(move || {
        let budget = Duration::from_secs(10);
        if pause_until_silent(&dest, budget) {
            tracing::info!("Desktop Spotify paused after delayed silent-wake hold");
        } else {
            tracing::warn!(
                "Desktop Spotify still playing after {budget:?}; restoring volume to avoid a stuck mute"
            );
        }
        drop(volume_guard);
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

fn mpris_set_volume(dest: &str, volume: f64) -> Result<()> {
    let status = Command::new("dbus-send")
        .args([
            "--session",
            "--type=method_call",
            &format!("--dest={dest}"),
            MPRIS_OBJECT,
            &format!("{DBUS_PROPERTIES}.Set"),
            &format!("string:{MPRIS_PLAYER}"),
            "string:Volume",
            &format!("variant:double:{volume}"),
        ])
        .status()
        .context("failed to set MPRIS Volume")?;

    if !status.success() {
        anyhow::bail!("MPRIS Set Volume failed with status {status}");
    }
    Ok(())
}

/// Parse `dbus-send --print-reply=literal` output for a double Volume.
fn parse_mpris_volume_reply(stdout: &str) -> Option<f64> {
    let text = stdout.trim();
    let after_double = text.split("double").nth(1)?;
    after_double
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn is_spotify_client_binary(binary: &str) -> bool {
    Path::new(binary)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "spotify")
}

fn spotify_sink_input_indices_from_json(json: &str) -> Vec<u32> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(inputs) = value.as_array() else {
        return Vec::new();
    };
    inputs
        .iter()
        .filter_map(|input| {
            let binary = input
                .get("properties")
                .and_then(|p| p.get("application.process.binary"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if !is_spotify_client_binary(binary) {
                return None;
            }
            input.get("index")?.as_u64().map(|i| i as u32)
        })
        .collect()
}

fn spotify_sink_input_indices() -> Vec<u32> {
    let output = Command::new("pactl")
        .args(["--format=json", "list", "sink-inputs"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    spotify_sink_input_indices_from_json(&String::from_utf8_lossy(&output.stdout))
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

/// Restore MPRIS Volume after a silent wake, including Play/OpenUri errors.
struct MprisVolumeGuard {
    dest: String,
    volume: f64,
}

impl MprisVolumeGuard {
    /// Zero volume only after a successful read so Drop always has a restore target.
    fn start(dest: &str) -> Option<Self> {
        let volume = match mpris_get_volume(dest) {
            Ok(volume) => volume,
            Err(err) => {
                tracing::warn!("Could not read MPRIS volume for silent wake: {err:#}");
                return None;
            }
        };
        if let Err(err) = mpris_set_volume(dest, 0.0) {
            tracing::warn!("Could not mute MPRIS volume for silent wake: {err:#}");
            return None;
        }
        Some(Self {
            dest: dest.to_string(),
            volume,
        })
    }
}

impl Drop for MprisVolumeGuard {
    fn drop(&mut self) {
        if let Err(err) = mpris_set_volume(&self.dest, self.volume) {
            tracing::warn!("Could not restore MPRIS volume after silent wake: {err:#}");
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
                for index in spotify_sink_input_indices() {
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
        if let Some(join) = self.join.take() {
            if let Ok(muted) = join.join() {
                for index in muted {
                    let _ = set_sink_input_mute(index, false);
                }
            }
        }
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
    fn relaunches_once_when_process_is_gone_before_mpris_is_ready() {
        assert!(should_relaunch_for_mpris(false, false, false));
        assert!(!should_relaunch_for_mpris(false, false, true));
        assert!(!should_relaunch_for_mpris(false, true, false));
        assert!(!should_relaunch_for_mpris(true, false, false));
    }

    #[test]
    fn parses_literal_mpris_volume_reply() {
        assert_eq!(
            parse_mpris_volume_reply("   variant       double 0.42\n"),
            Some(0.42)
        );
        assert_eq!(parse_mpris_volume_reply("double 1"), Some(1.0));
        assert_eq!(parse_mpris_volume_reply("not a volume"), None);
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

    #[test]
    fn extracts_spotify_sink_input_indices_from_pactl_json() {
        let json = r#"
        [
          {"index": 12, "properties": {"application.process.binary": "spotify"}},
          {"index": 13, "properties": {"application.process.binary": "spotify_player"}},
          {"index": 14, "properties": {"application.process.binary": "/usr/bin/spotify"}}
        ]
        "#;
        assert_eq!(spotify_sink_input_indices_from_json(json), vec![12, 14]);
        assert!(spotify_sink_input_indices_from_json("not json").is_empty());
    }
}
