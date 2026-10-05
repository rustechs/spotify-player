#![cfg(target_os = "linux")]

//! Capture system (PipeWire/Pulse) monitor audio for the spectrum visualizer
//! when playback is on an external Spotify Connect device.
//!
//! The local librespot `VisualizationSink` remains the preferred source. This
//! module only feeds `VisBands` while `local_sink_active` is false.

use crate::{
    config,
    state::SharedState,
    vis::{BandProcessor, VisBands},
};
use anyhow::{anyhow, Context, Result};
use libpulse_binding::{def::BufferAttr, sample, stream};
use libpulse_simple_binding::Simple;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Capture sample rate. `PipeWire`'s Pulse compat commonly runs at 48 kHz.
const CAPTURE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const BYTES_PER_SAMPLE: usize = 4; // f32
/// Stereo float frames per `Simple::read` call (~10 ms at 48 kHz).
const READ_FRAMES: usize = 480;
/// Bytes per read fragment. Pulse defaults `fragsize` to ~2s when left unset,
/// which leaves the visualizer draining buffered silence after playback starts.
const CAPTURE_FRAGSIZE: u32 = (READ_FRAMES * CHANNELS as usize * BYTES_PER_SAMPLE) as u32;
const RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How often the `auto` source is re-resolved while a capture stream is open.
const SOURCE_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn a background thread that taps the Pulse/PipeWire default-sink monitor
/// (or a configured source) and publishes FFT bands for the UI.
pub fn start(state: &SharedState) {
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
    let smoothing = configs.app_config.enable_audio_visualization_smoothing;

    if let Err(err) = std::thread::Builder::new()
        .name("system-audio-vis".to_string())
        .spawn(move || capture_loop(&bands, &source, smoothing))
    {
        tracing::error!("Failed to spawn system-audio visualization thread: {err:#}");
    } else {
        tracing::info!("Started system-audio visualization capture thread");
    }
}

/// Retry pacing for the capture thread: warn once per distinct error, then
/// debug, doubling the wait up to `MAX_RETRY_DELAY` so an absent Pulse server
/// or a bad source name does not flood the log for the life of the process.
#[derive(Default)]
struct Backoff {
    delay: Option<Duration>,
    last_error: Option<String>,
}

impl Backoff {
    fn wait(&mut self, what: &str, err: &anyhow::Error) {
        let msg = format!("{err:#}");
        if self.last_error.as_deref() == Some(msg.as_str()) {
            tracing::debug!("system-audio-vis: {what} failed again: {msg}");
        } else {
            tracing::warn!("system-audio-vis: {what} failed: {msg}; retrying with back-off");
            self.last_error = Some(msg);
        }
        let delay = self.delay.unwrap_or(RETRY_DELAY);
        std::thread::sleep(delay);
        self.delay = Some(next_retry_delay(delay));
    }

    fn reset(&mut self) {
        self.delay = None;
        self.last_error = None;
    }
}

fn next_retry_delay(current: Duration) -> Duration {
    (current * 2).min(MAX_RETRY_DELAY)
}

fn capture_loop(bands: &Arc<Mutex<VisBands>>, source_cfg: &str, smoothing: bool) {
    let mut processor = BandProcessor::new(Arc::clone(bands), CAPTURE_RATE as f32, smoothing);
    let mut simple: Option<Simple> = None;
    let mut current_source: Option<String> = None;
    let mut was_capturing = false;
    let mut raw = vec![0u8; READ_FRAMES * CHANNELS as usize * BYTES_PER_SAMPLE];
    let mut backoff = Backoff::default();
    let mut next_source_check = Instant::now();

    loop {
        if bands.lock().local_sink_active {
            if was_capturing {
                simple = None;
                current_source = None;
                // Buffer-only: the local sink owns the source flags from here on.
                processor.reset_buffer();
                was_capturing = false;
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        }

        // Resolve the source only when (re)opening or on a coarse timer. With
        // `auto` this shells out to `pactl`, which must not run per 10 ms read.
        let resolved = if simple.is_none() || Instant::now() >= next_source_check {
            next_source_check = Instant::now() + SOURCE_RECHECK_INTERVAL;
            Some(resolve_source(source_cfg))
        } else {
            None
        };
        match resolved {
            Some(Ok(desired_source))
                if current_source.as_deref() != Some(desired_source.as_str()) =>
            {
                simple = None;
                match open_capture(&desired_source) {
                    Ok(s) => {
                        tracing::info!("system-audio-vis: capturing from '{desired_source}'");
                        simple = Some(s);
                        current_source = Some(desired_source);
                        backoff.reset();
                    }
                    Err(err) => {
                        current_source = None;
                        backoff.wait(&format!("open '{desired_source}'"), &err);
                        continue;
                    }
                }
            }
            Some(Err(err)) if simple.is_none() => {
                backoff.wait("resolve capture source", &err);
                continue;
            }
            Some(Err(err)) => {
                tracing::debug!(
                    "system-audio-vis: keeping the current source; re-resolve failed: {err:#}"
                );
            }
            _ => {}
        }

        let Some(ref stream) = simple else {
            std::thread::sleep(RETRY_DELAY);
            continue;
        };

        if bands.lock().local_sink_active {
            continue;
        }

        if let Err(err) = stream.read(&mut raw) {
            simple = None;
            current_source = None;
            backoff.wait("read", &anyhow!("{err}"));
            continue;
        }
        backoff.reset();

        if bands.lock().local_sink_active {
            continue;
        }

        // Interleaved native-endian float32 stereo (`FLOAT32NE`) → mono.
        let (frames, _) = raw.as_chunks::<{ BYTES_PER_SAMPLE * 2 }>();
        processor.push_mono_samples(frames.iter().map(|frame| {
            let l = f32::from_ne_bytes([frame[0], frame[1], frame[2], frame[3]]);
            let r = f32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]);
            f32::midpoint(l, r)
        }));

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

    // Keep the record fragment near one read (~10 ms). The Pulse default is on
    // the order of two seconds, which shows up as a dead gap after the intro
    // decay while old silence is still being drained from the capture buffer.
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CAPTURE_FRAGSIZE,
    };

    Simple::new(
        None,
        "spotify-player",
        stream::Direction::Record,
        Some(source),
        "System audio visualization",
        &spec,
        None,
        Some(&attr),
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

#[cfg(test)]
mod tests {
    use super::{next_retry_delay, Duration, MAX_RETRY_DELAY, RETRY_DELAY};

    #[test]
    fn retry_delay_doubles_up_to_the_cap() {
        assert_eq!(next_retry_delay(RETRY_DELAY), Duration::from_secs(1));
        assert_eq!(next_retry_delay(Duration::from_secs(20)), MAX_RETRY_DELAY);
        assert_eq!(next_retry_delay(MAX_RETRY_DELAY), MAX_RETRY_DELAY);
    }
}
