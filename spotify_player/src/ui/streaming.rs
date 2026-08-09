use crate::state::SharedState;
use crate::vis::{decay_for_elapsed, peak_decay_for_elapsed, BandProcessor, VisBands};
use librespot_playback::{
    audio_backend::{Sink, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use parking_lot::Mutex;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Bar, BarChart, BarGroup},
    Frame,
};
use std::sync::Arc;

/// Height (in terminal rows) reserved for the audio visualization bar chart.
pub const VIS_HEIGHT: u16 = 8;

/// An audio sink wrapper that computes real-time FFT frequency bands from the
/// decoded audio stream and exposes them via a shared buffer for the UI.
///
/// It forwards every audio packet unchanged to the real backend, so playback
/// is not affected.
pub struct VisualizationSink {
    inner: Box<dyn Sink>,
    processor: BandProcessor,
}

impl VisualizationSink {
    /// Create a new `VisualizationSink` wrapping `inner`.
    ///
    /// `sample_rate` should match the actual librespot audio format sample rate
    /// (44100 or 48000 Hz) so that hop-based decay timings are accurate.
    pub fn new(inner: Box<dyn Sink>, bands: Arc<Mutex<VisBands>>, sample_rate: f32) -> Self {
        Self {
            inner,
            processor: BandProcessor::new(bands, sample_rate),
        }
    }
}

impl Sink for VisualizationSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Zero out the bands and reset normalization when playback stops so the
        // bars fall to silence and the next session starts with a fresh baseline.
        self.processor.reset();
        self.inner.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if let AudioPacket::Samples(ref samples) = packet {
            // Samples are interleaved stereo (L, R, L, R, …); mix down to mono f32.
            self.processor.push_mono_samples(samples.chunks(2).map(|c| {
                if c.len() == 2 {
                    f64::midpoint(c[0], c[1]) as f32
                } else {
                    c[0] as f32
                }
            }));
        }

        self.inner.write(packet, converter)
    }
}

/// Maps a normalised amplitude [0, 1] to an RGB colour.
/// Quiet (0.0) → cool blue, medium → green, loud (1.0) → hot red.
fn bar_color(t: f32) -> Color {
    let (r, g, b) = if t < 0.5 {
        let s = t * 2.0;
        (
            (30.0 + 20.0 * s) as u8,
            (100.0 + 155.0 * s) as u8,
            (255.0 * (1.0 - s * 0.5)) as u8,
        )
    } else {
        let s = (t - 0.5) * 2.0;
        (
            (50.0 + 205.0 * s) as u8,
            (255.0 * (1.0 - s)) as u8,
            (128.0 * (1.0 - s)) as u8,
        )
    };
    Color::Rgb(r, g, b)
}

/// Render a frequency-band bar chart using live FFT data from the audio sink.
///
/// Bars are subsampled to the available rect width so they always fill the area
/// cleanly. Heights use a sqrt (perceptual) curve so quiet signals stay visible.
/// Each bar is coloured by its amplitude: cool blue (quiet) → green → hot red (loud).
pub fn render_audio_visualization(frame: &mut Frame, state: &SharedState, rect: Rect) {
    // display_decay interpolates bar heights smoothly between write() calls.
    // We normalise against peak_envelope (NOT the per-frame peak), so display_decay
    // no longer cancels out and bars genuinely fade between audio packets.
    //
    // vis_bands is only Some when enable_audio_visualization is true.
    let Some(vis_lock) = state.vis_bands.as_ref() else {
        return;
    };
    let guard = vis_lock.lock();
    if !guard.is_active {
        return;
    }
    let display_decay = decay_for_elapsed(guard.updated_at.elapsed());
    let peak_norm =
        (guard.peak_envelope * peak_decay_for_elapsed(guard.updated_at.elapsed())).max(1e-6);
    // Copy the fixed-size array by value — no heap allocation.
    let values = guard.values;
    drop(guard);
    let num_bars = (rect.width as usize).min(values.len()).max(1);
    // Multiply by 8 to use ratatui's eighth-block characters (▁▂▃▄▅▆▇█),
    // giving 8× the resolution of whole terminal rows.
    let max_val = u64::from(rect.height) * 8;

    let step = values.len() as f64 / num_bars as f64;
    let bars: Vec<Bar> = (0..num_bars)
        .map(|i| {
            let idx = ((i as f64 * step) as usize).min(values.len() - 1);
            // Normalise against the slow peak envelope, then apply inter-frame decay.
            // Sqrt (gamma 0.5) scaling boosts quiet signals without clipping louds.
            let norm = ((values[idx] * display_decay) / peak_norm)
                .clamp(0.0, 1.0)
                .powf(0.5);
            let val = (norm * max_val as f32) as u64;
            Bar::default()
                .value(val)
                .text_value("")
                .style(Style::default().fg(bar_color(norm)))
        })
        .collect();

    let chart = BarChart::default()
        .data(BarGroup::default().bars(&bars))
        .bar_width(1)
        .bar_gap(0)
        .max(max_val);

    frame.render_widget(chart, rect);
}
