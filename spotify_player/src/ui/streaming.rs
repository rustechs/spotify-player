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
///
/// Playback metadata above the chart is inset one column left of this axis.
pub const Y_AXIS_WIDTH: u16 = 3;
/// Right margin for the x-axis end cap and `Hz` unit label.
const X_AXIS_UNIT_WIDTH: u16 = 2;

const DB_GRID_TICKS: [i32; 3] = [-12, -24, -36];
const DB_LABEL_TICKS: [i32; 4] = [0, -12, -24, -36];
/// Frequencies with a vertical grid line: the decades, evenly spaced on the
/// log axis.
const FREQ_GRID_HZ: [f32; 3] = [100.0, 1_000.0, 10_000.0];
/// Frequencies labelled on the x-axis, in the order they claim space: a chart
/// too narrow for all of them keeps the decades and drops the ones in between.
const FREQ_LABELS_HZ: [f32; 9] = [
    100.0, 1_000.0, 10_000.0, 50.0, 500.0, 5_000.0, 20_000.0, 200.0, 2_000.0,
];
/// Band peak below which we treat the monitor as silent (pause / idle). Band
/// levels are relative to a full-scale sine, so this is -100 dBFS.
const VIZ_SIGNAL_FLOOR: f32 = 1e-5;

/// Whether the UI should draw live bar heights from `VisBands`.
///
/// Bars follow monitor audio as soon as there is measurable signal, without
/// waiting for the Spotify API `is_playing` flag (which can lag by seconds).
/// When the API says paused and the monitor is silent, bars stay flat unless
/// a full-scale intro decay is still in progress.
fn should_show_viz_bars(guard: &VisBands, playback_is_playing: bool) -> bool {
    if guard.intro_level().is_some() {
        return true;
    }

    if !guard.is_active {
        return false;
    }

    let raw_peak = guard.values.iter().copied().fold(0.0_f32, f32::max);
    if raw_peak > VIZ_SIGNAL_FLOOR {
        return true;
    }

    playback_is_playing
}

fn playable_item_key(item: &rspotify::model::PlayableItem) -> Option<String> {
    match item {
        rspotify::model::PlayableItem::Track(track) => {
            track.id.as_ref().map(rspotify::prelude::Id::uri)
        }
        rspotify::model::PlayableItem::Episode(episode) => {
            Some(rspotify::prelude::Id::uri(&episode.id))
        }
        rspotify::model::PlayableItem::Unknown(_) => None,
    }
}

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
    pub fn new(
        inner: Box<dyn Sink>,
        bands: Arc<Mutex<VisBands>>,
        sample_rate: f32,
        smoothing: bool,
    ) -> Self {
        Self {
            inner,
            processor: BandProcessor::new(bands, sample_rate, smoothing),
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

/// Linearly interpolates between two RGB colors; selects the nearest endpoint
/// when either color is not an RGB color (e.g. an ANSI palette color).
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => Color::Rgb(
            (f32::from(r1) + (f32::from(r2) - f32::from(r1)) * t) as u8,
            (f32::from(g1) + (f32::from(g2) - f32::from(g1)) * t) as u8,
            (f32::from(b1) + (f32::from(b2) - f32::from(b1)) * t) as u8,
        ),
        (a, b) => {
            if t < 0.5 {
                a
            } else {
                b
            }
        }
    }
}

/// Maps a normalised amplitude `t` in [0, 1] to a color between the theme's
/// `low` (quiet), `mid` (medium) and `high` (loud) visualization stops.
fn bar_color(t: f32, low: Color, mid: Color, high: Color) -> Color {
    if t < 0.5 {
        lerp_color(low, mid, t * 2.0)
    } else {
        lerp_color(mid, high, (t - 0.5) * 2.0)
    }
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

fn freq_tick_x(plot_rect: Rect, freq_hz: f32) -> u16 {
    let fraction = freq_to_x_fraction(freq_hz);
    plot_rect.x + (fraction * f32::from(plot_rect.width.saturating_sub(1))).round() as u16
}

/// The band value drawn in plot column `column` of `columns`: the tallest of
/// the bands the column covers. Sampling one band per column would skip the
/// rest once there are fewer columns than bands, and a peak one band wide
/// could then be drawn from its neighbour or not at all.
fn column_value(values: &[f32; crate::vis::NUM_BANDS], column: usize, columns: usize) -> f32 {
    let step = values.len() as f64 / columns as f64;
    let first = ((column as f64 * step) as usize).min(values.len() - 1);
    let end = (((column + 1) as f64 * step) as usize).clamp(first + 1, values.len());
    values[first..end].iter().copied().fold(0.0, f32::max)
}

/// Where each frequency label starts on the x-axis row. Labels sit under
/// their tick, pulled in at the ends of the axis, and one that would touch a
/// label already placed is left out.
fn x_axis_labels(plot_rect: Rect, axis_cap_x: u16) -> Vec<(u16, String)> {
    let mut placed: Vec<(u16, String)> = Vec::new();
    for freq in FREQ_LABELS_HZ {
        let label = format_freq_hz(freq);
        let len = label.len() as u16;
        let Some(last_start) = axis_cap_x.checked_sub(len).filter(|&x| x >= plot_rect.x) else {
            continue;
        };
        let x = freq_tick_x(plot_rect, freq)
            .saturating_sub(len / 2)
            .clamp(plot_rect.x, last_start);
        let touches =
            |&(other, ref text): &(u16, String)| x <= other + text.len() as u16 && other <= x + len;
        if !placed.iter().any(touches) {
            placed.push((x, label));
        }
    }
    placed
}

fn db_tick_y(plot_rect: Rect, max_val: u64, db: i32) -> u16 {
    let norm = db_to_norm(db as f32);
    let bar_rows = ((norm * max_val as f32) / 8.0).round() as u16;
    plot_rect.y + plot_rect.height.saturating_sub(bar_rows.max(1))
}

/// Which plot cells the rendered bars occupy, so the frame and grid drawn
/// after the chart never overwrite a bar (and the chart's blank cells never
/// erase them).
struct BarCoverage {
    plot: Rect,
    /// Bar value per plot column, in eighths of a row (`0..=height * 8`).
    values: Vec<u64>,
}

impl BarCoverage {
    fn covers(&self, x: u16, y: u16) -> bool {
        if x < self.plot.x || x >= self.plot.right() || y < self.plot.y || y >= self.plot.bottom() {
            return false;
        }
        let column = usize::from(x - self.plot.x);
        let rows_below = u64::from(self.plot.bottom() - 1 - y);
        self.values
            .get(column)
            .is_some_and(|&value| value > rows_below * 8)
    }
}

fn render_axis_frame(frame: &mut Frame, chart_rect: Rect, style: Style, bars: &BarCoverage) {
    let buf = frame.buffer_mut();
    if chart_rect.width < 2 || chart_rect.height < 2 {
        return;
    }

    let left = chart_rect.x;
    let right = chart_rect.right().saturating_sub(1);
    let top = chart_rect.y;
    let bottom = chart_rect.bottom().saturating_sub(1);

    for x in left..right {
        if !bars.covers(x, top) {
            buf.set_string(x, top, "─", style);
        }
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
    style: Style,
    bars: &BarCoverage,
) {
    let buf = frame.buffer_mut();

    for db in DB_GRID_TICKS {
        let y = db_tick_y(plot_rect, max_val, db);
        if y <= plot_rect.y || y >= plot_rect.bottom() {
            continue;
        }
        for x in plot_rect.x..plot_rect.right() {
            if !bars.covers(x, y) {
                buf.set_string(x, y, "┄", style);
            }
        }
    }

    for freq in FREQ_GRID_HZ {
        let x = freq_tick_x(plot_rect, freq);
        if x <= plot_rect.x || x >= plot_rect.right() {
            continue;
        }
        for y in plot_rect.y..plot_rect.bottom() {
            if !bars.covers(x, y) {
                buf.set_string(x, y, "┆", style);
            }
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
    style: Style,
) {
    let buf = frame.buffer_mut();
    let label_y = chart_rect.bottom().saturating_sub(1);

    let axis_cap_x = chart_rect.right().saturating_sub(1);
    for (x, label) in x_axis_labels(plot_rect, axis_cap_x) {
        buf.set_string(x, label_y, &label, style);
    }

    if hz_margin.width >= 2 {
        let x = hz_margin.right().saturating_sub(2);
        buf.set_string(x, label_y, "Hz", style);
    }
}

/// Render a frequency-band bar chart using live FFT data from the audio sink.
///
/// The bands are fitted to the available rect width, one bar per column, so
/// they always fill the area cleanly. Heights use a sqrt (perceptual) curve so
/// quiet signals stay visible.
/// Each bar is coloured by its amplitude using the theme's `visualization`
/// colors: `low` (quiet) → `mid` → `high` (loud).
pub fn render_audio_visualization(
    frame: &mut Frame,
    state: &SharedState,
    theme: &Theme,
    rect: Rect,
) {
    let Some(vis_lock) = state.vis_bands.as_ref() else {
        return;
    };

    // Read player metadata before taking `vis_bands` so we never nest
    // player under vis (system-audio holds vis) while the UI already holds `ui`.
    let (playing_key, playback_is_playing) = {
        let player = state.player.read();
        let key = player.currently_playing().and_then(playable_item_key);
        let playing = player.playback.as_ref().is_some_and(|p| p.is_playing)
            || player
                .buffered_playback
                .as_ref()
                .is_some_and(|p| p.is_playing);
        (key, playing)
    };

    // Arm a full-scale (0 dB) intro as soon as track metadata is on screen so
    // bars appear with the axes, then fall with the usual render-side decay.
    {
        let mut guard = vis_lock.lock();
        match playing_key {
            Some(key) => guard.arm_intro_for_item(&key),
            None => guard.clear_intro(),
        }
    }

    let guard = vis_lock.lock();
    let intro_level = guard.intro_level();
    let mut values = if should_show_viz_bars(&guard, playback_is_playing) {
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
    if let Some(level) = intro_level {
        for v in &mut values {
            *v = (*v).max(level);
        }
    }
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

    // One bar per plot column: bands are merged on narrow terminals and
    // repeated on wide ones, so the axis labels always span the same width.
    let num_bars = usize::from(plot_rect.width).max(1);
    let max_val = u64::from(plot_rect.height) * 8;
    let vis_colors = theme.visualization();

    let bar_values: Vec<(u64, f32)> = (0..num_bars)
        .map(|i| {
            let norm = column_value(&values, i, num_bars);
            let val = (norm * max_val as f32).round() as u64;
            let val = if norm > 0.0 { val.max(1) } else { 0 };
            (val, norm)
        })
        .collect();
    let bars: Vec<Bar> = bar_values
        .iter()
        .map(|&(val, norm)| {
            Bar::default()
                .value(val)
                .text_value("")
                .style(Style::default().fg(bar_color(
                    norm,
                    vis_colors.low,
                    vis_colors.mid,
                    vis_colors.high,
                )))
        })
        .collect();

    let chart = BarChart::default()
        .data(BarGroup::default().bars(&bars))
        .bar_width(1)
        .bar_gap(0)
        .max(max_val);

    // The chart writes blank cells above every bar, so the frame and grid go
    // on top of it, skipping the cells the bars occupy.
    frame.render_widget(chart, plot_rect);
    let coverage = BarCoverage {
        plot: plot_rect,
        values: bar_values.iter().map(|&(val, _)| val).collect(),
    };
    render_axis_frame(frame, chart_rect, axis_style, &coverage);
    render_grid_lines(frame, plot_rect, max_val, axis_style, &coverage);
    render_y_axis_labels(frame, horiz[0], plot_rect, max_val, axis_style);
    render_x_axis_labels(frame, plot_rect, chart_rect, hz_margin, axis_style);
}

#[cfg(test)]
mod tests {
    use super::{column_value, x_axis_labels, BarCoverage};
    use crate::vis::NUM_BANDS;
    use ratatui::layout::Rect;

    #[test]
    fn a_column_draws_the_tallest_band_it_covers() {
        let drawn = |band: usize, columns: usize| {
            let mut values = [0.0_f32; NUM_BANDS];
            values[band] = 1.0;
            (0..columns)
                .filter(|&c| column_value(&values, c, columns) > 0.0)
                .count()
        };
        // 77 plot columns is an 84-column playback pane: fewer columns than
        // bands, and still no band may go undrawn.
        for columns in [20, 77, 127] {
            assert!((0..NUM_BANDS).all(|band| drawn(band, columns) == 1));
        }
        // With room to spare every band gets its own column or several.
        assert!((0..NUM_BANDS).all(|band| drawn(band, NUM_BANDS) == 1));
        assert!((0..NUM_BANDS).all(|band| drawn(band, 2 * NUM_BANDS) == 2));
    }

    #[test]
    fn x_axis_labels_stay_apart_and_inside_the_axis() {
        for width in 4..=240_u16 {
            // Plot columns `1..=width`, end cap in the column after them.
            let plot = Rect::new(1, 0, width, 7);
            let cap = plot.right();
            let mut labels = x_axis_labels(plot, cap);
            labels.sort();
            for (x, text) in &labels {
                assert!(
                    *x >= plot.x && x + text.len() as u16 <= cap,
                    "{width}: {labels:?}"
                );
            }
            for pair in labels.windows(2) {
                let (x, text) = &pair[0];
                assert!(x + (text.len() as u16) < pair[1].0, "{width}: {labels:?}");
            }
        }
    }

    #[test]
    fn x_axis_labels_keep_the_decades_when_space_runs_out() {
        let texts = |width: u16| {
            let plot = Rect::new(1, 0, width, 7);
            let mut labels = x_axis_labels(plot, plot.right());
            labels.sort();
            labels.into_iter().map(|(_, text)| text).collect::<Vec<_>>()
        };
        assert_eq!(
            texts(77),
            ["50", "100", "200", "500", "1k", "2k", "5k", "10k", "20k"]
        );
        let narrow = texts(24);
        for decade in ["100", "1k", "10k"] {
            assert!(narrow.iter().any(|text| text == decade), "{narrow:?}");
        }
        assert!(narrow.len() < 9, "{narrow:?}");
    }

    #[test]
    fn bar_coverage_marks_only_cells_under_bars() {
        // 4 columns, 3 rows: values are eighths of a row, bottom row is y = 2.
        let coverage = BarCoverage {
            plot: Rect::new(1, 0, 4, 3),
            values: vec![0, 8, 12, 24],
        };
        assert!(!coverage.covers(1, 2), "zero bar covers nothing");
        assert!(coverage.covers(2, 2));
        assert!(
            !coverage.covers(2, 1),
            "one-row bar stops at the bottom row"
        );
        assert!(
            coverage.covers(3, 1),
            "partial second row counts as covered"
        );
        assert!(!coverage.covers(3, 0));
        assert!(coverage.covers(4, 0), "full bar reaches the top row");
        assert!(!coverage.covers(0, 2), "outside the plot");
        assert!(!coverage.covers(5, 2));
        assert!(!coverage.covers(2, 3));
    }
}
