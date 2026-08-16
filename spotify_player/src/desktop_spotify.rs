//! Launch and MPRIS-nudge the official Spotify desktop client (Linux).
//!
//! Spotify Connect often omits the desktop app from `/v1/me/player/devices`
//! until it has joined a playback session. With `enable_streaming = "Never"`,
//! that leaves `spotify_player` unable to transfer until the user hits play in
//! the GUI. This module starts the client if needed and wakes it via MPRIS
//! (`Play` / `OpenUri`) so Connect can see it.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::config::DesktopSpotifyConfig;

const DBUS_DEST_BUS: &str = "org.freedesktop.DBus";
const DBUS_OBJECT: &str = "/org/freedesktop/DBus";
const MPRIS_OBJECT: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// Ensure the desktop Spotify client is running and has an active playback
/// session so it appears as a Connect device.
pub async fn ensure_awake(config: &DesktopSpotifyConfig, nudge_uri: Option<&str>) -> Result<()> {
    if !config.enable {
        return Ok(());
    }

    let dest = config.mpris_dest.as_str();

    if !mpris_name_has_owner(dest)? {
        if spotify_process_running() {
            tracing::info!(
                "Desktop Spotify process found but MPRIS not ready; waiting up to {}s",
                config.ready_timeout_secs
            );
        } else {
            tracing::info!(
                "Desktop Spotify MPRIS not present; launching `{}`",
                config.command
            );
            launch(config)?;
        }
        wait_for_mpris(dest, Duration::from_secs(config.ready_timeout_secs)).await?;
    }

    nudge(dest, nudge_uri, config.pause_after_nudge)?;
    Ok(())
}

fn launch(config: &DesktopSpotifyConfig) -> Result<()> {
    let program = resolve_command(&config.command)?;
    Command::new(&program)
        .args(&config.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

async fn wait_for_mpris(dest: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let poll = Duration::from_millis(400);
    loop {
        if mpris_name_has_owner(dest)? {
            tracing::info!("Desktop Spotify MPRIS ready ({dest})");
            return Ok(());
        }
        if start.elapsed() >= timeout {
            anyhow::bail!("timed out after {timeout:?} waiting for desktop Spotify MPRIS ({dest})");
        }
        tokio::time::sleep(poll).await;
    }
}

fn nudge(dest: &str, nudge_uri: Option<&str>, pause_after: bool) -> Result<()> {
    let uri = nudge_uri
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_spotify_uri);

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
        tracing::info!("Pausing desktop Spotify after wake nudge");
        let _ = mpris_call(dest, "Pause");
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
}
