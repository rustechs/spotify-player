//! Shared audio-visualization FFT state and processor.
//!
//! Lives outside `ui` so both the librespot sink wrapper and system-audio
//! capture can publish into the same band buffer without a UI dependency.

use parking_lot::Mutex;
use rustfft::{num_complex::Complex, FftPlanner};
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

const FFT_SIZE: usize = 1024;
/// Number of new samples consumed per FFT frame (overlap = `FFT_SIZE` - `HOP_SIZE`).
/// At 44100 Hz: 128 samples ≈ 2.9 ms between updates.
const HOP_SIZE: usize = 128;
/// Mono samples required before the first zero-padded FFT hop (~one Pulse read at
/// 48 kHz) so bars appear without waiting for a full 1024-sample window.
const WARM_START_MIN_SAMPLES: usize = HOP_SIZE;
pub const NUM_BANDS: usize = 128;

/// Per-FFT-frame decay multiplier for individual bands.
/// At 44100 Hz / `HOP_SIZE` 128 ≈ 344 hops/s, 0.985^~151 ≈ 1% in ~0.44 s — snappy
/// enough to track transients, not so slow that it smears.
const DECAY_FACTOR: f32 = 0.985;
/// Slower decay for the peak envelope used for normalization. At 344 hops/s,
/// 0.9985^x = 0.01 → x ≈ 1535 hops → ~4.5 s. The envelope stays elevated
/// through quiet passages so the bars reflect genuine relative loudness instead
/// of always filling to 100%.
const DECAY_FACTOR_PEAK: f32 = 0.9985;
/// Reference sample rate for the render-side decay helpers and the initial
/// `VisBands::sample_rate`; audio processors carry their own rate (44.1/48 kHz).
pub const SAMPLE_RATE: f32 = 44_100.0;

/// Shared frequency-band state exposed between the audio sink and the UI.
/// Storing `updated_at` lets the render function apply smooth time-based decay
/// independent of how often `write()` is called by the audio backend.
pub struct VisBands {
    /// Fixed-size array of per-band magnitudes.
    /// Using `[f32; NUM_BANDS]` instead of `Vec<f32>` means the render-frame
    /// copy (`let values = guard.values;`) is a plain stack copy with no heap
    /// allocation.
    pub values: [f32; NUM_BANDS],
    /// Wall-clock timestamp of the last `write()` hop that updated `values`.
    /// The render function reads this to compute time-based inter-frame decay
    /// without needing to be called at a fixed rate.
    pub updated_at: Instant,
    /// Slow-decaying peak envelope used to normalise bar heights.
    /// Rises instantly to any louder value; decays with `DECAY_FACTOR_PEAK`.
    /// Kept separate from per-band values so quiet passages look genuinely
    /// quieter — the VU «breathes» with the music.
    pub peak_envelope: f32,
    /// Set to `true` when any visualization audio source is live (local
    /// librespot sink and/or system-audio capture). The UI keeps the viz pane
    /// reserved whenever a track is loaded; this flag selects live band data
    /// versus an idle zero baseline (e.g. while paused).
    pub is_active: bool,
    /// Set to `true` only while the integrated librespot sink is playing.
    /// System-audio capture yields whenever this is true so the two sources
    /// never fight over `values`.
    pub local_sink_active: bool,
    /// PCM sample rate (Hz) of the active visualization source; used for axis labels.
    pub sample_rate: f32,
    /// PROTOTYPE: how `values` are laid out along the frequency axis.
    pub axis: AxisScale,
    /// Playable-item key the full-scale intro was armed for (track/episode id).
    /// Re-armed whenever the loaded item changes so bars flash with each new track.
    intro_item_key: Option<String>,
    /// Wall-clock start of the full-scale (0 dB) intro decay. `None` when idle.
    intro_started_at: Option<Instant>,
}

impl VisBands {
    pub fn new() -> Self {
        Self {
            values: [0.0f32; NUM_BANDS],
            updated_at: Instant::now(),
            peak_envelope: 1e-6,
            is_active: false,
            local_sink_active: false,
            sample_rate: SAMPLE_RATE,
            axis: AxisScale::Bins,
            intro_item_key: None,
            intro_started_at: None,
        }
    }

    /// Arm a full-scale decaying intro for `item_key` if it is a new playable item.
    ///
    /// The UI blends this with live FFT data so bars appear immediately with the
    /// axes, then fall using the same render-side decay as music response.
    ///
    /// Also resets the peak envelope so a stale peak from the previous item
    /// cannot keep fresh bars near the floor for several seconds.
    pub fn arm_intro_for_item(&mut self, item_key: &str) {
        if self.intro_item_key.as_deref() == Some(item_key) {
            return;
        }
        self.intro_item_key = Some(item_key.to_string());
        self.intro_started_at = Some(Instant::now());
        self.values.fill(0.0);
        self.peak_envelope = 1e-6;
        self.updated_at = Instant::now();
    }

    /// Clear intro state when no track is loaded.
    pub fn clear_intro(&mut self) {
        self.intro_item_key = None;
        self.intro_started_at = None;
    }

    /// Current intro bar height in `[0, 1]`, or `None` if the intro has finished.
    pub fn intro_level(&self) -> Option<f32> {
        let started = self.intro_started_at?;
        let level = decay_for_elapsed(started.elapsed());
        if level < 1e-3 {
            None
        } else {
            Some(level)
        }
    }
}

impl Default for VisBands {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the compound `DECAY_FACTOR` multiplier for the given elapsed wall-clock
/// duration.
///
/// Used **only on the render side** (`render_audio_visualization`) to interpolate
/// bar heights smoothly between audio-sink updates. It uses the fixed `SAMPLE_RATE`
/// reference (44 100 Hz); audio processors inline their own calculation using
/// their `sample_rate` so both sides are independently accurate.
pub fn decay_for_elapsed(elapsed: std::time::Duration) -> f32 {
    let elapsed_hops = elapsed.as_secs_f32() * SAMPLE_RATE / HOP_SIZE as f32;
    DECAY_FACTOR.powf(elapsed_hops)
}

/// Returns the compound `DECAY_FACTOR_PEAK` multiplier for the given elapsed
/// wall-clock duration.
///
/// Used **only on the render side** to decay the peak-envelope estimate between
/// audio packets. See `decay_for_elapsed` for the render-vs-sink split.
pub fn peak_decay_for_elapsed(elapsed: std::time::Duration) -> f32 {
    let elapsed_hops = elapsed.as_secs_f32() * SAMPLE_RATE / HOP_SIZE as f32;
    DECAY_FACTOR_PEAK.powf(elapsed_hops)
}

/// How frequencies map onto the bar axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AxisScale {
    /// Bars follow `precompute_band_ranges`: one FFT bin each at the low end,
    /// log-spaced only near the top.
    Bins,
    /// Bars are log-spaced between `f_min` and `f_max` (Hz).
    Log { f_min: f32, f_max: f32 },
}

/// PROTOTYPE: a second, longer window used for the low end of a log axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LowBand {
    pub window: usize,
    /// Below this the long window is used alone; above `blend_to_hz` the short
    /// one is. In between the two are cross-faded in log frequency.
    pub blend_from_hz: f32,
    pub blend_to_hz: f32,
}

/// PROTOTYPE: selectable analysis so layout variants can be compared.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Analysis {
    /// Samples under the Hann window.
    pub window: usize,
    /// FFT length; larger than `window` means zero padding.
    pub fft: usize,
    pub low: Option<LowBand>,
    pub axis: AxisScale,
    /// Offline harness: decay by exactly one hop per hop instead of wall-clock time.
    pub simulated_clock: bool,
}

impl Analysis {
    pub const LOG_AXIS: AxisScale = AxisScale::Log {
        f_min: 40.0,
        f_max: 20_000.0,
    };

    /// What `main` does today.
    pub fn current() -> Self {
        Self {
            window: FFT_SIZE,
            fft: FFT_SIZE,
            low: None,
            axis: AxisScale::Bins,
            simulated_clock: false,
        }
    }

    /// A: log bars from the current 1024-sample window, interpolated between bins.
    pub fn log_1024() -> Self {
        Self {
            axis: Self::LOG_AXIS,
            ..Self::current()
        }
    }

    /// B: the same 1024-sample window zero-padded to a 4096-point FFT.
    pub fn log_1024_padded() -> Self {
        Self {
            fft: 4096,
            ..Self::log_1024()
        }
    }

    /// C: log bars from a 4096-sample window.
    pub fn log_4096() -> Self {
        Self {
            window: 4096,
            fft: 4096,
            ..Self::log_1024()
        }
    }

    /// D: a 4096-sample window below about 1 kHz, the 1024-sample one above.
    pub fn log_dual() -> Self {
        Self {
            low: Some(LowBand {
                window: 4096,
                blend_from_hz: 600.0,
                blend_to_hz: 1_200.0,
            }),
            ..Self::log_1024()
        }
    }

    /// PROTOTYPE: `SPOTIFY_PLAYER_VIS_PROTO=A|B|C|D` selects a variant at runtime.
    pub fn from_env() -> Self {
        match std::env::var("SPOTIFY_PLAYER_VIS_PROTO").as_deref() {
            Ok("A") => Self::log_1024(),
            Ok("B") => Self::log_1024_padded(),
            Ok("C") => Self::log_4096(),
            Ok("D") => Self::log_dual(),
            _ => Self::current(),
        }
    }
}

/// One Hann-windowed FFT with reusable buffers. Magnitudes are scaled so a
/// full-scale sine reads 1.0 whatever the window length.
struct Spectrum {
    window: usize,
    fft: Arc<dyn rustfft::Fft<f32>>,
    hann: Vec<f32>,
    buf: Vec<Complex<f32>>,
    mags: Vec<f32>,
    gain: f32,
}

impl Spectrum {
    fn new(window: usize, fft_len: usize) -> Self {
        let hann: Vec<f32> = (0..window)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (window - 1) as f32).cos())
            })
            .collect();
        let gain = 2.0 / hann.iter().sum::<f32>();
        Self {
            window,
            fft: FftPlanner::<f32>::new().plan_fft_forward(fft_len),
            hann,
            buf: vec![Complex::new(0.0, 0.0); fft_len],
            mags: vec![0.0; fft_len / 2],
            gain,
        }
    }

    /// Analyze the most recent `window` of the first `available` queued samples,
    /// zero-filled when fewer are available.
    fn analyze(&mut self, queue: &VecDeque<f32>, available: usize) {
        let (front, back) = queue.as_slices();
        let offset = available.saturating_sub(self.window);
        for (i, dst) in self.buf.iter_mut().enumerate() {
            let j = offset + i;
            let s = if i < self.window && j < available {
                let sample = if j < front.len() {
                    front[j]
                } else {
                    back[j - front.len()]
                };
                sample * self.hann[i]
            } else {
                0.0
            };
            *dst = Complex::new(s, 0.0);
        }
        self.fft.process(&mut self.buf);
        for (mag, c) in self.mags.iter_mut().zip(self.buf.iter()) {
            *mag = c.norm() * self.gain;
        }
    }
}

/// Where each bar reads the spectrum.
enum Layout {
    Bins(Vec<(usize, usize)>),
    /// Fractional bin edges per bar for the primary spectrum and, when there is
    /// a long window, for it too; `high_weight` cross-fades between them.
    Log {
        edges: Vec<(f32, f32)>,
        low_edges: Vec<(f32, f32)>,
        high_weight: [f32; NUM_BANDS],
    },
}

/// Shared FFT processor that turns mono PCM into frequency bands.
///
/// Used by both the local librespot `VisualizationSink` and (optionally) the
/// system-audio capture path, so Connect-controlled desktop Spotify can drive
/// the same UI bars.
pub struct BandProcessor {
    /// Ring-buffer of mono f32 samples waiting to be processed.
    sample_buf: VecDeque<f32>,
    /// Shared state written every hop and read by the UI render thread.
    bands: Arc<Mutex<VisBands>>,
    primary: Spectrum,
    low: Option<Spectrum>,
    layout: Layout,
    analysis: Analysis,
    /// Samples needed before a full hop: the longest window.
    needed: usize,
    /// Actual audio sample rate in Hz.
    sample_rate: f32,
    new_bands: [f32; NUM_BANDS],
    smooth_scratch: [f32; NUM_BANDS],
    /// When true, emit one zero-padded FFT hop as soon as `WARM_START_MIN_SAMPLES`
    /// are buffered instead of waiting for a full window.
    warm_start: bool,
    hops: u64,
}

impl BandProcessor {
    /// Create a processor that publishes into `bands`.
    pub fn new(bands: Arc<Mutex<VisBands>>, sample_rate: f32) -> Self {
        Self::with_analysis(bands, sample_rate, Analysis::from_env())
    }

    /// PROTOTYPE: create a processor with an explicit analysis variant.
    pub fn with_analysis(
        bands: Arc<Mutex<VisBands>>,
        sample_rate: f32,
        analysis: Analysis,
    ) -> Self {
        let primary = Spectrum::new(analysis.window, analysis.fft);
        let low = analysis.low.map(|l| Spectrum::new(l.window, l.window));
        let layout = match analysis.axis {
            AxisScale::Bins => Layout::Bins(precompute_band_ranges(analysis.fft / 2, NUM_BANDS)),
            AxisScale::Log { f_min, f_max } => {
                let edge = |i: usize| f_min * (f_max / f_min).powf(i as f32 / NUM_BANDS as f32);
                let to_bins = |fft_len: usize| -> Vec<(f32, f32)> {
                    let bin_hz = sample_rate / fft_len as f32;
                    (0..NUM_BANDS)
                        .map(|b| (edge(b) / bin_hz, edge(b + 1) / bin_hz))
                        .collect()
                };
                let mut high_weight = [1.0_f32; NUM_BANDS];
                if let Some(l) = analysis.low {
                    for (b, w) in high_weight.iter_mut().enumerate() {
                        let center = (edge(b) * edge(b + 1)).sqrt();
                        *w = ((center / l.blend_from_hz).ln()
                            / (l.blend_to_hz / l.blend_from_hz).ln())
                        .clamp(0.0, 1.0);
                    }
                }
                Layout::Log {
                    edges: to_bins(analysis.fft),
                    low_edges: analysis.low.map(|l| to_bins(l.window)).unwrap_or_default(),
                    high_weight,
                }
            }
        };
        let needed = analysis.window.max(analysis.low.map_or(0, |l| l.window));
        Self {
            sample_buf: VecDeque::with_capacity(needed * 2),
            bands,
            primary,
            low,
            layout,
            analysis,
            needed,
            sample_rate,
            new_bands: [0.0f32; NUM_BANDS],
            smooth_scratch: [0.0f32; NUM_BANDS],
            warm_start: true,
            hops: 0,
        }
    }

    /// PROTOTYPE harness: the bands of the last hop, before decay and peak hold.
    #[cfg(test)]
    pub fn last_frame(&self) -> [f32; NUM_BANDS] {
        self.new_bands
    }

    /// PROTOTYPE harness: hops run so far.
    #[cfg(test)]
    pub fn hops(&self) -> u64 {
        self.hops
    }

    /// Request a zero-padded first FFT hop on the next batch (e.g. after pause).
    #[cfg(all(feature = "system-audio-visualization", target_os = "linux"))]
    pub fn mark_warm_start(&mut self) {
        self.warm_start = true;
    }

    /// Push mono f32 samples and publish any completed FFT hops.
    pub fn push_mono_samples<I>(&mut self, samples: I)
    where
        I: IntoIterator<Item = f32>,
    {
        self.sample_buf.extend(samples);
        self.process_hops();
    }

    /// Local sink stopped: clear queued samples, zero the published bands and
    /// release the source flags. The intro is left alone; the UI clears it
    /// when nothing is loaded, and clearing it here re-armed a full-scale
    /// flash on every pause.
    pub fn reset(&mut self) {
        self.reset_buffer();
        let mut g = self.bands.lock();
        g.is_active = false;
        g.local_sink_active = false;
    }

    /// Stop contributing without touching the source flags or the intro:
    /// clears queued samples, zeroes the published bands and arms a warm
    /// start. The system-audio capture uses this when it yields to the local
    /// sink, which owns `local_sink_active` from then on.
    pub fn reset_buffer(&mut self) {
        let mut g = self.bands.lock();
        g.values.fill(0.0);
        g.peak_envelope = 1e-6;
        g.updated_at = Instant::now();
        drop(g);
        self.sample_buf.clear();
        self.warm_start = true;
    }

    fn process_hops(&mut self) {
        // Update vis_bands after EVERY hop (not at the end of the batch), so a
        // transient at the start of a packet is visible within one hop.
        if self.warm_start
            && self.sample_buf.len() >= WARM_START_MIN_SAMPLES
            && self.sample_buf.len() < self.needed
        {
            self.run_hop(self.sample_buf.len());
            self.warm_start = false;
        }

        while self.sample_buf.len() >= self.needed {
            self.run_hop(self.needed);
            self.warm_start = false;
        }
    }

    fn run_hop(&mut self, available: usize) {
        debug_assert!(available <= self.needed);
        self.primary.analyze(&self.sample_buf, available);
        if let Some(low) = self.low.as_mut() {
            low.analyze(&self.sample_buf, available);
        }

        match &self.layout {
            Layout::Bins(ranges) => {
                fill_log_bands(&self.primary.mags, ranges, &mut self.new_bands);
            }
            Layout::Log {
                edges,
                low_edges,
                high_weight,
            } => {
                for (b, out) in self.new_bands.iter_mut().enumerate() {
                    let high = band_rms(&self.primary.mags, edges[b].0, edges[b].1);
                    let w = high_weight[b];
                    *out = match &self.low {
                        Some(low) if w < 1.0 => {
                            let l = band_rms(&low.mags, low_edges[b].0, low_edges[b].1);
                            l * (1.0 - w) + high * w
                        }
                        _ => high,
                    };
                }
            }
        }
        smooth_bands(&mut self.new_bands, &mut self.smooth_scratch);

        // Apply decay since the last hop, then rise to any louder value.
        let mut g = self.bands.lock();
        let elapsed_hops = if self.analysis.simulated_clock {
            1.0
        } else {
            g.updated_at.elapsed().as_secs_f32() * self.sample_rate / HOP_SIZE as f32
        };
        let decay = DECAY_FACTOR.powf(elapsed_hops);
        let peak_decay = DECAY_FACTOR_PEAK.powf(elapsed_hops);
        let frame_peak = self.new_bands.iter().copied().fold(0.0_f32, f32::max);
        for (stored, fresh) in g.values.iter_mut().zip(self.new_bands.iter()) {
            *stored = (*stored * decay).max(*fresh);
        }
        g.peak_envelope = (g.peak_envelope * peak_decay).max(frame_peak);
        g.sample_rate = self.sample_rate;
        g.axis = self.analysis.axis;
        g.updated_at = Instant::now();
        g.is_active = true;
        drop(g);

        self.hops += 1;
        self.sample_buf.drain(..HOP_SIZE);
    }
}

/// RMS magnitude of a spectrum between two fractional bin positions, from the
/// piecewise-linear interpolant of the power spectrum. A bar narrower than one
/// bin degenerates to the interpolated magnitude at its position. Bin 0 (DC)
/// is never read.
fn band_rms(mags: &[f32], x_lo: f32, x_hi: f32) -> f32 {
    let last = (mags.len() - 1) as f32;
    let lo = x_lo.clamp(1.0, last);
    let hi = x_hi.clamp(lo, last);
    let power_at = |x: f32| {
        let i = (x.floor() as usize).min(mags.len() - 1);
        let next = (i + 1).min(mags.len() - 1);
        let t = x - i as f32;
        let a = mags[i] * mags[i];
        let b = mags[next] * mags[next];
        a + (b - a) * t
    };
    if hi - lo < 1e-6 {
        return power_at(lo).sqrt();
    }
    let mut area = 0.0_f32;
    let mut x = lo;
    while x < hi {
        let next = (x.floor() + 1.0).min(hi);
        area += 0.5 * (power_at(x) + power_at(next)) * (next - x);
        x = next;
    }
    (area / (hi - lo)).sqrt()
}

/// Precomputes the `(start, end)` FFT bin ranges for each log-scale band.
///
/// Called once in `BandProcessor::new()`; the result is stored and
/// reused every hop so processing never runs `powf` per band per frame.
/// Bin 0 (DC component) is skipped by starting `used_up_to` at 1.
fn precompute_band_ranges(num_bins: usize, num_bands: usize) -> Vec<(usize, usize)> {
    let log_min = 1.0_f64;
    let log_max = num_bins as f64;
    let mut used_up_to: usize = 1;
    let mut ranges = Vec::with_capacity(num_bands);
    for band in 0..num_bands {
        if used_up_to >= num_bins {
            // All bins exhausted — pad remaining bands with a silent dummy range.
            ranges.push((num_bins - 1, num_bins));
            continue;
        }
        let t_start = band as f64 / num_bands as f64;
        let t_end = (band + 1) as f64 / num_bands as f64;
        let natural_start = (log_min * (log_max / log_min).powf(t_start)) as usize;
        let natural_end = (log_min * (log_max / log_min).powf(t_end)) as usize;
        // Advance past already-used bins so low-frequency bands do not all
        // share the same FFT bin and produce an identical flat plateau.
        let start = natural_start.max(used_up_to).min(num_bins - 1);
        let end = natural_end.max(start + 1).min(num_bins);
        used_up_to = end;
        ranges.push((start, end));
    }
    ranges
}

/// Fills `out` with the RMS magnitude of each log-scale band using the
/// precomputed bin ranges — no `Vec` allocation and no `powf` per call.
fn fill_log_bands(magnitudes: &[f32], band_ranges: &[(usize, usize)], out: &mut [f32]) {
    for (band_val, &(start, end)) in out.iter_mut().zip(band_ranges.iter()) {
        let len = (end - start) as f32;
        let sum_sq: f32 = magnitudes[start..end].iter().map(|&v| v * v).sum();
        *band_val = (sum_sq / len).sqrt();
    }
}

/// Applies a single pass of 3-point weighted smoothing [0.25, 0.5, 0.25] across
/// adjacent bands to reduce per-bin jitter without blurring transients.
///
/// `scratch` is a caller-supplied buffer (same length as `bands`) used as a
/// temporary copy, avoiding a `Vec` allocation on every hop.
fn smooth_bands(bands: &mut [f32], scratch: &mut [f32]) {
    let n = bands.len();
    if n < 3 {
        return;
    }
    scratch[..n].copy_from_slice(&bands[..n]);
    for i in 0..n {
        let prev = scratch[if i > 0 { i - 1 } else { 0 }];
        let next = scratch[if i + 1 < n { i + 1 } else { n - 1 }];
        bands[i] = prev * 0.25 + scratch[i] * 0.5 + next * 0.25;
    }
}

/// Band layout shared by every processor (and the axis labels).
fn band_ranges() -> &'static [(usize, usize)] {
    static RANGES: OnceLock<Vec<(usize, usize)>> = OnceLock::new();
    RANGES.get_or_init(|| precompute_band_ranges(FFT_SIZE / 2, NUM_BANDS))
}

/// Horizontal fraction in `[0, 1]` where `freq_hz` sits on the bar axis.
///
/// Bars are indexed by band, and the low bands cover consecutive linear FFT
/// bins (see `precompute_band_ranges`), so labels must follow the band layout
/// rather than a pure log scale: 1 kHz at 44.1 kHz is band ~22, not the middle.
pub fn freq_to_x_fraction(freq_hz: f32, sample_rate: f32, axis: AxisScale) -> f32 {
    if let AxisScale::Log { f_min, f_max } = axis {
        return ((freq_hz.max(f_min) / f_min).ln() / (f_max / f_min).ln()).clamp(0.0, 1.0);
    }
    let ranges = band_ranges();
    let bin = freq_hz.max(0.0) / (sample_rate / FFT_SIZE as f32);
    let num_bands = ranges.len() as f32;
    for (i, &(start, end)) in ranges.iter().enumerate() {
        if bin < end as f32 {
            let span = (end - start).max(1) as f32;
            let within = ((bin - start as f32) / span).clamp(0.0, 1.0);
            return ((i as f32 + within) / num_bands).clamp(0.0, 1.0);
        }
    }
    1.0
}

/// Convert dB relative to peak into the normalised bar scale used for rendering.
pub fn db_to_norm(db: f32) -> f32 {
    10_f32.powf(db / 40.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freq_to_x_fraction_maps_endpoints() {
        let rate = 44_100.0;
        let first_bin_hz = rate / FFT_SIZE as f32;
        assert!((freq_to_x_fraction(first_bin_hz, rate, AxisScale::Bins) - 0.0).abs() < 1e-6);
        assert!((freq_to_x_fraction(rate / 2.0, rate, AxisScale::Bins) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn freq_to_x_fraction_follows_the_band_layout() {
        let rate = 44_100.0;
        let ticks = [100.0, 500.0, 1_000.0, 5_000.0, 10_000.0, 20_000.0];
        let xs: Vec<f32> = ticks
            .iter()
            .map(|&f| freq_to_x_fraction(f, rate, AxisScale::Bins))
            .collect();
        assert!(xs.windows(2).all(|w| w[0] < w[1]), "{xs:?}");
        // 1 kHz is FFT bin ~23, i.e. band ~22 of 128 under the linear low end.
        let one_k = freq_to_x_fraction(1_000.0, rate, AxisScale::Bins);
        assert!((0.15..0.25).contains(&one_k), "{one_k}");
        // The band a label lands in must actually contain that frequency's bin.
        let bin = 1_000.0 / (rate / FFT_SIZE as f32);
        let band = (one_k * NUM_BANDS as f32) as usize;
        let (start, end) = band_ranges()[band];
        assert!(
            (start as f32..end as f32).contains(&bin),
            "{start}..{end} vs {bin}"
        );
    }

    #[test]
    fn db_to_norm_matches_render_curve() {
        assert!((db_to_norm(0.0) - 1.0).abs() < 1e-6);
        assert!(db_to_norm(-40.0) < db_to_norm(-12.0));
    }
}
