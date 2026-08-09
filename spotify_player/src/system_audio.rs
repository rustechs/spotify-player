//! Capture system (PipeWire/Pulse) monitor audio for the spectrum visualizer
//! when playback is on an external Spotify Connect device.
//!
//! The local librespot `VisualizationSink` remains the preferred source. This
//! module only feeds `VisBands` while `local_sink_active` is false.

use crate::{
    config,
    state::SharedState,
    ui::streaming::{BandProcessor, VisBands},
};
use anyhow::{anyhow, Context, Result};
use libpulse_binding::{sample, stream};
use libpulse_simple_binding::Simple;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// Capture sample rate. `PipeWire`'s Pulse compat commonly runs at 48 kHz.
const CAPTURE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const BYTES_PER_SAMPLE: usize = 4; // f32
/// Stereo float frames per `Simple::read` call (~10 ms at 48 kHz).
const READ_FRAMES: usize = 480;
const RETRY_DELAY: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_millis(100);

/// Spawn a background thread that taps the Pulse/PipeWire default-sink monitor
/// (or a configured source) and publishes FFT bands for the UI.
pub fn start(state: SharedState) {
    let configs = config::get_config();
    if !configs.app_config.enable_audio_visualization
        || !configs.app_config.enable_system_audio_visualization
    {
        return;
    }
    let Some(bands) = state.vis_bands.as_ref().map(Arc::clone) else {
        return;
    };
    let source = configs.app_config.system_audio_source.clone();

    if let Err(err) = std::thread::Builder::new()
        .name("system-audio-vis".to_string())
        .spawn(move || capture_loop(&state, &bands, &source))
    {
        tracing::error!("Failed to spawn system-audio visualization thread: {err:#}");
    } else {
        tracing::info!("Started system-audio visualization capture thread");
    }
}

fn capture_loop(state: &SharedState, bands: &Arc<Mutex<VisBands>>, source_cfg: &str) {
    let mut processor = BandProcessor::new(Arc::clone(bands), CAPTURE_RATE as f32);
    let mut simple: Option<Simple> = None;
    let mut current_source: Option<String> = None;
    let mut was_capturing = false;
    let mut raw = vec![0u8; READ_FRAMES * CHANNELS as usize * BYTES_PER_SAMPLE];

    loop {
        let local_active = bands.lock().local_sink_active;
        let playing = {
            let player = state.player.read();
            player
                .buffered_playback
                .as_ref()
                .is_some_and(|p| p.is_playing)
                || player.playback.as_ref().is_some_and(|p| p.is_playing)
        };

        let should_capture = !local_active && playing;

        if !should_capture {
            if was_capturing {
                // Drop the stream so we don't keep a monitor node open while idle.
                simple = None;
                current_source = None;
                // Only clear activity if the local sink has not taken over.
                let mut g = bands.lock();
                if !g.local_sink_active {
                    g.is_active = false;
                }
                was_capturing = false;
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        }

        let desired_source = match resolve_source(source_cfg) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("system-audio-vis: failed to resolve capture source: {err:#}");
                std::thread::sleep(RETRY_DELAY);
                continue;
            }
        };

        if current_source.as_deref() != Some(desired_source.as_str()) {
            simple = None;
            match open_capture(&desired_source) {
                Ok(s) => {
                    tracing::info!("system-audio-vis: capturing from '{desired_source}'");
                    simple = Some(s);
                    current_source = Some(desired_source);
                }
                Err(err) => {
                    tracing::warn!(
                        "system-audio-vis: failed to open '{desired_source}': {err:#}; retrying"
                    );
                    current_source = None;
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
            }
        }

        let Some(ref stream) = simple else {
            std::thread::sleep(RETRY_DELAY);
            continue;
        };

        // Re-check before blocking on read so a local sink takeover is noticed quickly.
        if bands.lock().local_sink_active {
            continue;
        }

        if let Err(err) = stream.read(&mut raw) {
            tracing::warn!("system-audio-vis: read failed: {err}; reopening stream");
            simple = None;
            current_source = None;
            std::thread::sleep(RETRY_DELAY);
            continue;
        }

        if bands.lock().local_sink_active {
            continue;
        }

        // Interleaved float32 LE stereo → mono.
        processor.push_mono_samples(raw.chunks_exact(BYTES_PER_SAMPLE * 2).map(|frame| {
            let l = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
            let r = f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
            0.5 * (l + r)
        }));

        {
            let mut g = bands.lock();
            if !g.local_sink_active {
                g.is_active = true;
            }
        }
        was_capturing = true;
    }
}

fn open_capture(source: &str) -> Result<Simple> {
    let spec = sample::Spec {
        format: sample::Format::FLOAT32NE,
        channels: CHANNELS,
        rate: CAPTURE_RATE,
    };
    if !spec.is_valid() {
        return Err(anyhow!("invalid Pulse sample spec"));
    }

    Simple::new(
        None,
        "spotify-player",
        stream::Direction::Record,
        Some(source),
        "System audio visualization",
        &spec,
        None,
        None,
    )
    .map_err(|e| anyhow!("Pulse Simple::new: {e}"))
}

/// Resolve `auto` to `<default-sink>.monitor`, or return the configured name.
fn resolve_source(configured: &str) -> Result<String> {
    let trimmed = configured.trim();
    if !trimmed.is_empty() && trimmed != "auto" {
        return Ok(trimmed.to_string());
    }

    // Prefer pactl — widely available with PipeWire's Pulse compat and avoids
    // standing up a full async Pulse mainloop just to read the default sink.
    let output = std::process::Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .context("run pactl get-default-sink")?;
    if !output.status.success() {
        return Err(anyhow!(
            "pactl get-default-sink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let sink = String::from_utf8(output.stdout)
        .context("decode pactl stdout")?
        .trim()
        .to_string();
    if sink.is_empty() {
        return Err(anyhow!("empty default sink from pactl"));
    }
    Ok(format!("{sink}.monitor"))
}
