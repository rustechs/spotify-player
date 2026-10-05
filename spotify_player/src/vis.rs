//! Shared audio-visualization FFT state and processor.
//!
//! Lives outside `ui` so both the librespot sink wrapper and system-audio
//! capture can publish into the same band buffer without a UI dependency.

use parking_lot::Mutex;
use rustfft::{num_complex::Complex, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// Analysis window for everything above the crossover: short, so a hit rises
/// within a few hops.
const SHORT_WINDOW: usize = 1024;
/// Analysis window for the bass, where the short one cannot tell two notes
/// apart: its bins are 43-47 Hz wide.
const LONG_WINDOW: usize = 4096;
/// Number of new samples consumed per FFT frame.
/// At 44100 Hz: 128 samples ≈ 2.9 ms between updates.
const HOP_SIZE: usize = 128;
pub const NUM_BANDS: usize = 128;
/// Frequency range of the band axis. Bands are log-spaced between the two, so
/// every octave is equally wide whatever the sample rate.
pub const AXIS_MIN_HZ: f32 = 40.0;
pub const AXIS_MAX_HZ: f32 = 20_000.0;
/// Bands below `CROSSOVER_LOW_HZ` read the long window and bands above
/// `CROSSOVER_HIGH_HZ` the short one; in between the two are cross-faded.
const CROSSOVER_LOW_HZ: f32 = 600.0;
const CROSSOVER_HIGH_HZ: f32 = 1_200.0;

/// Per-FFT-frame decay multiplier for individual bands.
/// At 44100 Hz / `HOP_SIZE` 128 ≈ 344 hops/s, 0.985^~151 ≈ 1% in ~0.44 s — snappy
/// enough to track transients, not so slow that it smears.
const DECAY_FACTOR: f32 = 0.985;
/// Slower decay for the peak envelope used for normalization. At 344 hops/s,
/// 0.9985^x = 0.01 → x ≈ 1535 hops → ~4.5 s. The envelope stays elevated
/// through quiet passages so the bars reflect genuine relative loudness instead
/// of always filling to 100%.
const DECAY_FACTOR_PEAK: f32 = 0.9985;
/// Reference sample rate for the render-side decay helpers; audio processors
/// carry their own rate (44.1/48 kHz).
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

/// One Hann-windowed FFT with reusable buffers.
struct Spectrum {
    hann: Vec<f32>,
    fft: Arc<dyn rustfft::Fft<f32>>,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Magnitude per bin, scaled by `gain`.
    mags: Vec<f32>,
    /// Scales magnitudes so a full-scale sine reads 1.0 whatever the window
    /// length. The two windows must report one level for one tone, or the
    /// cross-fade between them shows as a step.
    gain: f32,
}

impl Spectrum {
    fn new(len: usize) -> Self {
        let hann: Vec<f32> = (0..len)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (len - 1) as f32).cos()))
            .collect();
        let fft = FftPlanner::<f32>::new().plan_fft_forward(len);
        Self {
            gain: 2.0 / hann.iter().sum::<f32>(),
            hann,
            buf: vec![Complex::new(0.0, 0.0); len],
            scratch: vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()],
            mags: vec![0.0; len / 2],
            fft,
        }
    }

    /// Analyse the window of `samples` that ends at index `end`, zero-padded
    /// when fewer samples than a window precede it.
    fn analyze(&mut self, samples: &VecDeque<f32>, end: usize) {
        let start = end.saturating_sub(self.hann.len());
        let recent = samples.range(start..end);
        for ((dst, &w), &sample) in self.buf.iter_mut().zip(&self.hann).zip(recent) {
            *dst = Complex::new(sample * w, 0.0);
        }
        for dst in &mut self.buf[end - start..] {
            *dst = Complex::new(0.0, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        for (mag, c) in self.mags.iter_mut().zip(&self.buf) {
            *mag = c.norm() * self.gain;
        }
    }
}

/// Where one band reads the two spectra.
struct Band {
    /// Fractional bin range in the short and in the long spectrum.
    short_bins: (f32, f32),
    long_bins: (f32, f32),
    /// Share of the short window in this band.
    short_weight: f32,
}

/// Share of the short window for a band centred on `center_hz`: 0 below the
/// crossover, 1 above it, rising evenly in pitch in between.
fn short_weight(center_hz: f32) -> f32 {
    ((center_hz / CROSSOVER_LOW_HZ).ln() / (CROSSOVER_HIGH_HZ / CROSSOVER_LOW_HZ).ln())
        .clamp(0.0, 1.0)
}

/// Lower edge of band `band` in Hz; `band == NUM_BANDS` is the top of the axis.
fn band_edge_hz(band: usize) -> f32 {
    AXIS_MIN_HZ * (AXIS_MAX_HZ / AXIS_MIN_HZ).powf(band as f32 / NUM_BANDS as f32)
}

/// Shared FFT processor that turns mono PCM into log-spaced frequency bands.
///
/// Used by both the local librespot `VisualizationSink` and (optionally) the
/// system-audio capture path, so Connect-controlled desktop Spotify can drive
/// the same UI bars.
pub struct BandProcessor {
    /// Recent mono samples, oldest first: the `analysed` ones the last hop
    /// ended on, then those still waiting for a hop.
    samples: VecDeque<f32>,
    /// Length of the analysed prefix of `samples`; at most `LONG_WINDOW`
    /// between hops.
    analysed: usize,
    /// Shared state written every hop and read by the UI render thread.
    bands: Arc<Mutex<VisBands>>,
    short: Spectrum,
    long: Spectrum,
    layout: Vec<Band>,
    /// Actual audio sample rate in Hz — used for precise hop-based decay
    /// calculation, since librespot / Pulse can run at 44100 or 48000 Hz.
    sample_rate: f32,
    /// Whether each band is blended with its two neighbours.
    smoothing: bool,
    new_bands: [f32; NUM_BANDS],
    smooth_scratch: [f32; NUM_BANDS],
}

impl BandProcessor {
    /// Create a processor that publishes into `bands`.
    ///
    /// `sample_rate` should match the PCM source (44100 or 48000 Hz): it sets
    /// where each band reads the spectrum and how fast the bars decay.
    /// `smoothing` is the `enable_audio_visualization_smoothing` setting.
    pub fn new(bands: Arc<Mutex<VisBands>>, sample_rate: f32, smoothing: bool) -> Self {
        let layout = (0..NUM_BANDS)
            .map(|band| {
                let (lo, hi) = (band_edge_hz(band), band_edge_hz(band + 1));
                let bins = |window: usize| {
                    let per_hz = window as f32 / sample_rate;
                    (lo * per_hz, hi * per_hz)
                };
                Band {
                    short_bins: bins(SHORT_WINDOW),
                    long_bins: bins(LONG_WINDOW),
                    short_weight: short_weight((lo * hi).sqrt()),
                }
            })
            .collect();
        Self {
            samples: VecDeque::with_capacity(LONG_WINDOW * 2),
            analysed: 0,
            bands,
            short: Spectrum::new(SHORT_WINDOW),
            long: Spectrum::new(LONG_WINDOW),
            layout,
            sample_rate,
            smoothing,
            new_bands: [0.0f32; NUM_BANDS],
            smooth_scratch: [0.0f32; NUM_BANDS],
        }
    }

    /// Push mono f32 samples and publish any completed FFT hops.
    pub fn push_mono_samples<I>(&mut self, samples: I)
    where
        I: IntoIterator<Item = f32>,
    {
        self.samples.extend(samples);
        // One hop per `HOP_SIZE` new samples, from the first ones on: until a
        // full window of history exists the windows are zero-padded, so bars
        // appear at once instead of after the 85 ms the long window spans.
        //
        // `vis_bands` is updated after every hop, not once per batch, so a
        // transient at the start of a packet shows within one hop.
        while self.samples.len() - self.analysed >= HOP_SIZE {
            self.analysed += HOP_SIZE;
            self.run_hop();
            let stale = self.analysed.saturating_sub(LONG_WINDOW);
            self.samples.drain(..stale);
            self.analysed -= stale;
        }
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
    /// clears queued samples and zeroes the published bands. The system-audio
    /// capture uses this when it yields to the local sink, which owns
    /// `local_sink_active` from then on.
    pub fn reset_buffer(&mut self) {
        let mut g = self.bands.lock();
        g.values.fill(0.0);
        g.peak_envelope = 1e-6;
        g.updated_at = Instant::now();
        drop(g);
        self.samples.clear();
        self.analysed = 0;
    }

    /// Analyse the windows ending at `analysed` and publish the bands.
    fn run_hop(&mut self) {
        self.short.analyze(&self.samples, self.analysed);
        self.long.analyze(&self.samples, self.analysed);

        for (out, band) in self.new_bands.iter_mut().zip(&self.layout) {
            let weight = band.short_weight;
            let mut level = 0.0;
            if weight < 1.0 {
                level += (1.0 - weight) * band_level(&self.long.mags, band.long_bins);
            }
            if weight > 0.0 {
                level += weight * band_level(&self.short.mags, band.short_bins);
            }
            *out = level;
        }
        if self.smoothing {
            smooth_bands(&mut self.new_bands, &mut self.smooth_scratch);
        }

        // Apply wall-clock decay since the last hop, then rise to any louder value.
        // Use self.sample_rate for precision (may be 44100 or 48000 Hz).
        let mut g = self.bands.lock();
        let elapsed_hops =
            g.updated_at.elapsed().as_secs_f32() * self.sample_rate / HOP_SIZE as f32;
        let decay = DECAY_FACTOR.powf(elapsed_hops);
        let peak_decay = DECAY_FACTOR_PEAK.powf(elapsed_hops);
        let frame_peak = self.new_bands.iter().copied().fold(0.0_f32, f32::max);
        for (stored, fresh) in g.values.iter_mut().zip(self.new_bands.iter()) {
            *stored = (*stored * decay).max(*fresh);
        }
        g.peak_envelope = (g.peak_envelope * peak_decay).max(frame_peak);
        g.updated_at = Instant::now();
        g.is_active = true;
    }
}

/// Level of a spectrum between two fractional bin positions: the power under
/// the piecewise-linear interpolant of the squared magnitudes, as an amplitude.
///
/// Summing rather than averaging means a tone reads the same however many
/// bins its band spans, so levels do not droop where bands get wider. A band
/// narrower than one bin reads the interpolated bin instead. Bin 0 (DC) is
/// never read, and a band above the last bin reads zero.
fn band_level(mags: &[f32], (from, to): (f32, f32)) -> f32 {
    let last = (mags.len() - 1) as f32;
    if from >= last {
        return 0.0;
    }
    let lo = from.max(1.0);
    let hi = to.clamp(lo, last);
    let power_at = |x: f32| {
        let i = x as usize;
        let next = (i + 1).min(mags.len() - 1);
        let (a, b) = (mags[i] * mags[i], mags[next] * mags[next]);
        a + (b - a) * (x - i as f32)
    };
    if hi - lo < 1e-6 {
        return power_at(lo).sqrt();
    }
    let mut area = 0.0_f32;
    let mut x = lo;
    while x < hi {
        let next = (x.floor() + 1.0).min(hi);
        area += f32::midpoint(power_at(x), power_at(next)) * (next - x);
        x = next;
    }
    (area / (hi - lo).min(1.0)).sqrt()
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

/// Horizontal fraction in `[0, 1]` where `freq_hz` sits on the band axis.
pub fn freq_to_x_fraction(freq_hz: f32) -> f32 {
    ((freq_hz.max(AXIS_MIN_HZ) / AXIS_MIN_HZ).ln() / (AXIS_MAX_HZ / AXIS_MIN_HZ).ln())
        .clamp(0.0, 1.0)
}

/// Convert dB relative to peak into the normalised bar scale used for rendering.
pub fn db_to_norm(db: f32) -> f32 {
    10_f32.powf(db / 40.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [f32; 2] = [44_100.0, 48_000.0];

    fn tone(rate: f32, hz: f32, amplitude: f32, secs: f32) -> Vec<f32> {
        (0..(secs * rate) as usize)
            .map(|n| amplitude * (2.0 * std::f32::consts::PI * hz * n as f32 / rate).sin())
            .collect()
    }

    fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    /// Deterministic pink noise (Paul Kellet's economy filter over xorshift).
    fn pink(rate: f32, secs: f32) -> Vec<f32> {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let (mut b0, mut b1, mut b2) = (0.0_f32, 0.0_f32, 0.0_f32);
        (0..(secs * rate) as usize)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let white = (state >> 40) as f32 / (1_u64 << 23) as f32 - 1.0;
                b0 = 0.997_65 * b0 + white * 0.099_046;
                b1 = 0.963 * b1 + white * 0.296_516_4;
                b2 = 0.57 * b2 + white * 1.052_691_3;
                0.1 * (b0 + b1 + b2 + white * 0.184_8)
            })
            .collect()
    }

    /// The fresh bands of every hop over `pcm`, after a pre-roll of silence so
    /// that no hop reads a zero-padded window.
    fn frames(rate: f32, pcm: &[f32]) -> Vec<[f32; NUM_BANDS]> {
        frames_with(rate, pcm, true)
    }

    fn frames_with(rate: f32, pcm: &[f32], smoothing: bool) -> Vec<[f32; NUM_BANDS]> {
        let mut processor =
            BandProcessor::new(Arc::new(Mutex::new(VisBands::new())), rate, smoothing);
        processor.push_mono_samples(vec![0.0; LONG_WINDOW]);
        pcm.as_chunks::<HOP_SIZE>()
            .0
            .iter()
            .map(|hop| {
                processor.push_mono_samples(hop.iter().copied());
                processor.new_bands
            })
            .collect()
    }

    fn band_of(hz: f32) -> usize {
        ((freq_to_x_fraction(hz) * NUM_BANDS as f32) as usize).min(NUM_BANDS - 1)
    }

    /// Tallest of band `band` and its two neighbours.
    fn peak_near(frame: &[f32; NUM_BANDS], band: usize) -> f32 {
        frame[band.saturating_sub(1)..=(band + 1).min(NUM_BANDS - 1)]
            .iter()
            .copied()
            .fold(0.0, f32::max)
    }

    fn db(a: f32, b: f32) -> f32 {
        20.0 * (a.max(1e-12) / b.max(1e-12)).log10()
    }

    fn median(mut values: Vec<f32>) -> f32 {
        values.sort_by(f32::total_cmp);
        values[values.len() / 2]
    }

    /// Hops into a signal after which a steady tone has filled the long window.
    const SETTLED: usize = LONG_WINDOW / HOP_SIZE + 8;

    #[test]
    fn log_axis_gives_every_octave_the_same_width() {
        assert!(freq_to_x_fraction(AXIS_MIN_HZ).abs() < 1e-6);
        assert!((freq_to_x_fraction(AXIS_MAX_HZ) - 1.0).abs() < 1e-6);
        assert!(
            freq_to_x_fraction(1.0).abs() < 1e-6,
            "clamped below the axis"
        );
        assert!((freq_to_x_fraction(96_000.0) - 1.0).abs() < 1e-6);
        assert!((freq_to_x_fraction(1_000.0) - 0.518).abs() < 1e-3);
        let octave = freq_to_x_fraction(200.0) - freq_to_x_fraction(100.0);
        for hz in [50.0, 400.0, 3_200.0, 9_000.0] {
            let width = freq_to_x_fraction(2.0 * hz) - freq_to_x_fraction(hz);
            assert!((width - octave).abs() < 1e-5, "{hz}: {width} vs {octave}");
        }
    }

    #[test]
    fn crossover_fades_from_the_long_window_to_the_short_one() {
        assert!(short_weight(CROSSOVER_LOW_HZ).abs() < 1e-6);
        assert!(short_weight(100.0).abs() < 1e-6);
        assert!((short_weight(CROSSOVER_HIGH_HZ) - 1.0).abs() < 1e-6);
        assert!((short_weight(8_000.0) - 1.0).abs() < 1e-6);
        let inside: Vec<f32> = [650.0, 800.0, 950.0, 1_100.0]
            .iter()
            .map(|&hz| short_weight(hz))
            .collect();
        assert!(inside.windows(2).all(|w| w[0] < w[1]), "{inside:?}");
        assert!(inside.iter().all(|w| (0.0..1.0).contains(w)), "{inside:?}");
    }

    #[test]
    fn full_scale_sine_reads_one_in_both_windows() {
        let rate = 48_000.0;
        // 3 kHz is the centre of a bin in both windows at this rate.
        let pcm: VecDeque<f32> = tone(rate, 3_000.0, 1.0, 0.2).into();
        for len in [SHORT_WINDOW, LONG_WINDOW] {
            let mut spectrum = Spectrum::new(len);
            spectrum.analyze(&pcm, pcm.len());
            let peak = spectrum.mags.iter().copied().fold(0.0, f32::max);
            assert!((peak - 1.0).abs() < 0.01, "{len}: {peak}");
        }
    }

    #[test]
    fn band_level_does_not_depend_on_how_wide_the_band_is() {
        // A tone on bin 40: the Hann main lobe covers bins 38..42.
        let mut mags = vec![0.0_f32; 512];
        (mags[39], mags[40], mags[41]) = (0.5, 1.0, 0.5);
        let wide: Vec<f32> = [4.0_f32, 8.0, 17.0]
            .iter()
            .map(|half| band_level(&mags, (40.0 - half, 40.0 + half)))
            .collect();
        assert!(
            wide.windows(2).all(|w| db(w[0], w[1]).abs() < 0.01),
            "{wide:?}"
        );
        // A band narrower than one bin reads the interpolated bin; a summed
        // lobe sits 1.8 dB above its own peak.
        let narrow = band_level(&mags, (39.9, 40.1));
        assert!(db(narrow, 1.0).abs() < 0.5, "{narrow}");
        assert!(db(wide[0], narrow) < 2.5, "{wide:?} vs {narrow}");
    }

    #[test]
    fn band_level_ignores_dc_and_reads_zero_above_the_last_bin() {
        let mut mags = vec![0.0_f32; 512];
        mags[0] = 1.0;
        assert!(band_level(&mags, (0.2, 0.9)) < 1e-6, "below bin 1");
        mags[511] = 1.0;
        assert!(band_level(&mags, (511.0, 600.0)) < 1e-6);
        assert!(band_level(&mags, (600.0, 700.0)) < 1e-6);
    }

    #[test]
    fn tone_peaks_in_the_band_the_axis_gives_it() {
        for rate in RATES {
            for hz in [100.0, 440.0, 1_000.0, 5_000.0, 10_000.0] {
                let all = frames(rate, &tone(rate, hz, 0.5, 0.2));
                let frame = all.last().unwrap();
                let peak = (0..NUM_BANDS)
                    .max_by(|&a, &b| frame[a].total_cmp(&frame[b]))
                    .unwrap();
                let expected = freq_to_x_fraction(hz) * NUM_BANDS as f32;
                let off = peak as f32 + 0.5 - expected;
                assert!(off.abs() <= 1.0, "{hz} Hz at {rate}: {off} bands off");
            }
        }
    }

    #[test]
    fn two_bass_notes_a_fifth_apart_stay_separate() {
        for rate in RATES {
            let pcm = mix(&tone(rate, 100.0, 0.4, 0.3), &tone(rate, 150.0, 0.4, 0.3));
            let (low, high) = (band_of(100.0), band_of(150.0));
            // Unresolved tones beat, so one frame can show a dip that is gone
            // a few hops later: the shallowest dip is the one that counts.
            let shallowest = frames(rate, &pcm)[SETTLED..]
                .iter()
                .map(|frame| {
                    let dip = frame[low..=high].iter().copied().fold(f32::MAX, f32::min);
                    db(peak_near(frame, low).min(peak_near(frame, high)), dip)
                })
                .fold(f32::MAX, f32::min);
            assert!(shallowest >= 6.0, "{rate}: {shallowest} dB");
        }
    }

    #[test]
    fn equal_notes_read_alike_wherever_they_sit() {
        for rate in RATES {
            let reading = |hz: f32| {
                let pcm = mix(&tone(rate, 100.0, 0.4, 0.3), &tone(rate, hz, 0.4, 0.3));
                median(
                    frames(rate, &pcm)[SETTLED..]
                        .iter()
                        .map(|f| db(peak_near(f, band_of(hz)), peak_near(f, band_of(100.0))))
                        .collect(),
                )
            };
            let against_bass = [reading(1_000.0), reading(4_000.0), reading(10_000.0)];
            let highest = against_bass.iter().copied().fold(f32::MIN, f32::max);
            let lowest = against_bass.iter().copied().fold(f32::MAX, f32::min);
            assert!(highest - lowest <= 2.0, "{rate}: {against_bass:?}");
            assert!(
                against_bass.iter().all(|level| level.abs() <= 4.0),
                "{rate}: {against_bass:?}"
            );
        }
    }

    #[test]
    fn noise_reads_level_across_the_crossover() {
        for rate in RATES {
            let all = frames(rate, &pink(rate, 1.5));
            let rms_between = |lo_hz: f32, hi_hz: f32| {
                let bands = band_of(lo_hz)..=band_of(hi_hz);
                let (mut sum, mut count) = (0.0_f64, 0_u32);
                for frame in &all[SETTLED..] {
                    for level in &frame[bands.clone()] {
                        sum += f64::from(*level) * f64::from(*level);
                        count += 1;
                    }
                }
                (sum / f64::from(count)).sqrt() as f32
            };
            // Pink noise carries the same power in every octave, so the long
            // window below the crossover and the short one above must agree.
            let step = db(rms_between(1_200.0, 2_000.0), rms_between(350.0, 600.0));
            assert!(step.abs() <= 3.0, "{rate}: {step} dB");
        }
    }

    #[test]
    fn notes_above_the_crossover_rise_at_the_short_windows_speed() {
        for rate in RATES {
            let rise_ms = |hz: f32| {
                let all = frames(rate, &tone(rate, hz, 0.5, 0.25));
                let band = band_of(hz);
                let steady = peak_near(all.last().unwrap(), band);
                let hops = all
                    .iter()
                    .position(|frame| peak_near(frame, band) >= 0.5 * steady)
                    .unwrap();
                (hops + 1) as f32 * HOP_SIZE as f32 / rate * 1_000.0
            };
            // The long window alone needs 35 ms or more at any frequency.
            let (treble, faded, bass) = (rise_ms(5_000.0), rise_ms(1_000.0), rise_ms(60.0));
            assert!(treble <= 15.0, "{rate}: 5 kHz rises in {treble} ms");
            assert!(faded <= 20.0, "{rate}: 1 kHz rises in {faded} ms");
            assert!(bass <= 60.0, "{rate}: 60 Hz rises in {bass} ms");
        }
    }

    #[test]
    fn smoothing_spreads_a_lone_note_over_its_neighbours() {
        // At 8 kHz a band is far wider than a tone's main lobe, so the tone
        // sits in one band, or two when it straddles an edge.
        let pcm = tone(48_000.0, 8_000.0, 0.5, 0.2);
        let lit = |smoothing: bool| {
            let all = frames_with(48_000.0, &pcm, smoothing);
            let frame = all.last().unwrap();
            let peak = frame.iter().copied().fold(0.0, f32::max);
            frame.iter().filter(|&&level| level >= 0.3 * peak).count()
        };
        assert!(lit(false) <= 2, "sharp: {} bands", lit(false));
        assert!(lit(true) >= 3, "smooth: {} bands", lit(true));
    }

    #[test]
    fn bars_appear_with_the_first_hop_and_silence_stays_flat() {
        let bands = Arc::new(Mutex::new(VisBands::new()));
        let mut processor = BandProcessor::new(Arc::clone(&bands), 48_000.0, true);
        processor.push_mono_samples(vec![0.0; HOP_SIZE - 1]);
        assert!(!bands.lock().is_active, "less than one hop of samples");
        processor.push_mono_samples(vec![0.0; LONG_WINDOW]);
        assert!(bands.lock().is_active);
        assert!(bands.lock().values.iter().all(|v| *v == 0.0));

        processor.reset_buffer();
        processor.push_mono_samples(tone(48_000.0, 1_000.0, 0.5, 0.01)[..HOP_SIZE].to_vec());
        let first = bands.lock().values[band_of(1_000.0)];
        assert!(first > 0.0, "one hop of a tone must already show");
    }

    #[test]
    fn history_stays_one_long_window_deep() {
        let mut processor =
            BandProcessor::new(Arc::new(Mutex::new(VisBands::new())), 48_000.0, true);
        for _ in 0..40 {
            processor.push_mono_samples(vec![0.0; 480]);
            assert!(processor.analysed <= LONG_WINDOW);
            assert!(processor.samples.len() < LONG_WINDOW + HOP_SIZE);
        }
        assert_eq!(processor.analysed, LONG_WINDOW);
    }

    #[test]
    fn db_to_norm_matches_render_curve() {
        assert!((db_to_norm(0.0) - 1.0).abs() < 1e-6);
        assert!(db_to_norm(-40.0) < db_to_norm(-12.0));
    }
}
