use crate::config::Theme;
use crate::state::SharedState;
use crate::vis::{
    db_to_norm, decay_for_elapsed, freq_to_x_fraction, peak_decay_for_elapsed, BandProcessor,
    VisBands,
};
use librespot_playback::{
    audio_backend::{Sink, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use parking_lot::Mutex;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Bar, BarChart, BarGroup},
    Frame,
};
use std::sync::Arc;

/// Height (in terminal rows) reserved for the audio visualization bar chart.
pub const VIS_HEIGHT: u16 = 8;

/// Left margin for dB axis labels (`dB` + tick, 3 columns wide).
const Y_AXIS_WIDTH: u16 = 3;
/// Right margin for the x-axis end cap and `Hz` unit label.
const X_AXIS_UNIT_WIDTH: u16 = 2;

const DB_GRID_TICKS: [i32; 3] = [-12, -24, -36];
const DB_LABEL_TICKS: [i32; 4] = [0, -12, -24, -36];
const FREQ_TICKS_HZ: [f32; 6] = [100.0, 500.0, 1_000.0, 5_000.0, 10_000.0, 20_000.0];

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

fn axis_style(theme: &Theme) -> Style {
    let accent = theme.playback_progress_bar();
    Style::default().fg(accent.fg.unwrap_or(Color::Green))
}

fn plot_area(chart_rect: Rect) -> Rect {
    // Inset left for the y-axis line and right for the x-axis end cap (┘).
    Rect {
        x: chart_rect.x + 1,
        y: chart_rect.y,
        width: chart_rect.width.saturating_sub(2),
        height: chart_rect.height.saturating_sub(1),
    }
}

fn format_freq_hz(freq_hz: f32) -> String {
    if freq_hz >= 1_000.0 {
        format!("{:.0}k", (freq_hz / 1000.0).round())
    } else {
        format!("{freq_hz:.0}")
    }
}

fn format_db_label(db: i32) -> String {
    if db == 0 {
        "0".to_string()
    } else {
        format!("{db}")
    }
}

fn freq_tick_x(plot_rect: Rect, freq_hz: f32, sample_rate: f32) -> u16 {
    let fraction = freq_to_x_fraction(freq_hz, sample_rate);
    plot_rect.x + (fraction * f32::from(plot_rect.width.saturating_sub(1))).round() as u16
}

fn db_tick_y(plot_rect: Rect, max_val: u64, db: i32) -> u16 {
    let norm = db_to_norm(db as f32);
    let bar_rows = ((norm * max_val as f32) / 8.0).round() as u16;
    plot_rect.y + plot_rect.height.saturating_sub(bar_rows.max(1))
}

fn render_axis_frame(frame: &mut Frame, chart_rect: Rect, style: Style) {
    let buf = frame.buffer_mut();
    if chart_rect.width < 2 || chart_rect.height < 2 {
        return;
    }

    let left = chart_rect.x;
    let right = chart_rect.right().saturating_sub(1);
    let top = chart_rect.y;
    let bottom = chart_rect.bottom().saturating_sub(1);

    for x in left..right {
        buf.set_string(x, top, "─", style);
        buf.set_string(x, bottom, "─", style);
    }
    for y in top..=bottom {
        buf.set_string(left, y, "│", style);
    }

    buf.set_string(left, top, "┌", style);
    buf.set_string(left, bottom, "└", style);
    if bottom > top {
        buf.set_string(right, bottom, "┘", style);
    }
}

fn render_grid_lines(
    frame: &mut Frame,
    plot_rect: Rect,
    max_val: u64,
    sample_rate: f32,
    style: Style,
) {
    let buf = frame.buffer_mut();

    for db in DB_GRID_TICKS {
        let y = db_tick_y(plot_rect, max_val, db);
        if y <= plot_rect.y || y >= plot_rect.bottom() {
            continue;
        }
        for x in plot_rect.x..plot_rect.right() {
            buf.set_string(x, y, "┄", style);
        }
    }

    for freq in FREQ_TICKS_HZ {
        let x = freq_tick_x(plot_rect, freq, sample_rate);
        if x <= plot_rect.x || x >= plot_rect.right() {
            continue;
        }
        for y in plot_rect.y..plot_rect.bottom() {
            buf.set_string(x, y, "┆", style);
        }
    }
}

fn render_y_axis_labels(
    frame: &mut Frame,
    y_axis_rect: Rect,
    plot_rect: Rect,
    max_val: u64,
    style: Style,
) {
    let buf = frame.buffer_mut();

    if plot_rect.height > 0 {
        buf.set_string(y_axis_rect.x, plot_rect.y, "dB", style);
    }

    for db in DB_LABEL_TICKS {
        let y = db_tick_y(plot_rect, max_val, db);
        if y >= plot_rect.bottom() {
            continue;
        }
        let label = format_db_label(db);
        let x = if db == 0 {
            y_axis_rect.x + 2
        } else {
            y_axis_rect.x + y_axis_rect.width.saturating_sub(label.len() as u16)
        };
        buf.set_string(x, y, label, style);
    }
}

fn render_x_axis_labels(
    frame: &mut Frame,
    plot_rect: Rect,
    chart_rect: Rect,
    hz_margin: Rect,
    sample_rate: f32,
    style: Style,
) {
    let buf = frame.buffer_mut();
    let label_y = chart_rect.bottom().saturating_sub(1);

    for freq in FREQ_TICKS_HZ {
        let x = freq_tick_x(plot_rect, freq, sample_rate);
        let label = format_freq_hz(freq);
        let label_x = x.saturating_sub(label.len() as u16 / 2);
        let axis_cap_x = chart_rect.right().saturating_sub(1);
        if label_x + label.len() as u16 <= axis_cap_x {
            buf.set_string(label_x, label_y, &label, style);
        }
    }

    if hz_margin.width >= 2 {
        let x = hz_margin.right().saturating_sub(2);
        buf.set_string(x, label_y, "Hz", style);
    }
}

/// Render a frequency-band bar chart using live FFT data from the audio sink.
///
/// Bars are subsampled to the available rect width so they always fill the area
/// cleanly. Heights use a sqrt (perceptual) curve so quiet signals stay visible.
/// Each bar is coloured by its amplitude: cool blue (quiet) → green → hot red (loud).
pub fn render_audio_visualization(
    frame: &mut Frame,
    state: &SharedState,
    theme: &Theme,
    rect: Rect,
) {
    let Some(vis_lock) = state.vis_bands.as_ref() else {
        return;
    };
    let guard = vis_lock.lock();
    let sample_rate = guard.sample_rate;
    let values = if guard.is_active {
        let display_decay = decay_for_elapsed(guard.updated_at.elapsed());
        let peak_norm =
            (guard.peak_envelope * peak_decay_for_elapsed(guard.updated_at.elapsed())).max(1e-6);
        let mut normalised = guard.values;
        for v in &mut normalised {
            *v = ((*v * display_decay) / peak_norm).clamp(0.0, 1.0).powf(0.5);
        }
        normalised
    } else {
        [0.0f32; crate::vis::NUM_BANDS]
    };
    drop(guard);

    if rect.height < 2 || rect.width <= Y_AXIS_WIDTH + 1 {
        return;
    }

    let axis_style = axis_style(theme);

    let horiz =
        Layout::horizontal([Constraint::Length(Y_AXIS_WIDTH), Constraint::Fill(1)]).split(rect);
    let chart_parts =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(X_AXIS_UNIT_WIDTH)])
            .split(horiz[1]);
    let chart_rect = chart_parts[0];
    let hz_margin = chart_parts[1];
    let plot_rect = plot_area(chart_rect);

    if plot_rect.width == 0 || plot_rect.height == 0 {
        return;
    }

    let num_bars = (plot_rect.width as usize).min(values.len()).max(1);
    let max_val = u64::from(plot_rect.height) * 8;

    render_axis_frame(frame, chart_rect, axis_style);
    render_grid_lines(frame, plot_rect, max_val, sample_rate, axis_style);

    let step = values.len() as f64 / num_bars as f64;
    let bars: Vec<Bar> = (0..num_bars)
        .map(|i| {
            let idx = ((i as f64 * step) as usize).min(values.len() - 1);
            let norm = values[idx];
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

    frame.render_widget(chart, plot_rect);
    render_y_axis_labels(frame, horiz[0], plot_rect, max_val, axis_style);
    render_x_axis_labels(
        frame,
        plot_rect,
        chart_rect,
        hz_margin,
        sample_rate,
        axis_style,
    );
}
