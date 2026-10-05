//! PROTOTYPE harness (branch proto/visualizer-log-axis, not for main).
//!
//! Feeds the same synthetic 48 kHz PCM through every analysis variant, measures
//! placement, resolution, onset, release and cost, and renders each variant
//! through the real `render_audio_visualization` into ratatui's `TestBackend`.
//!
//! Run: `PROTO_OUT=<dir> cargo test [--release] ... vis_proto -- --ignored --nocapture`
use std::{collections::VecDeque, sync::Arc, time::Instant};

use parking_lot::Mutex;
use ratatui::{backend::TestBackend, layout::Rect, widgets::Paragraph, Terminal};

use crate::{
    config,
    state::{SharedState, State},
    vis::{freq_to_x_fraction, Analysis, AxisScale, BandProcessor, VisBands, NUM_BANDS},
};

const RATE: f32 = 48_000.0;
const HOP: usize = 128;
const PANEL_ROWS: u16 = super::streaming::VIS_HEIGHT;

fn variants() -> Vec<(&'static str, &'static str, Analysis)> {
    let sim = |a: Analysis| Analysis {
        simulated_clock: true,
        ..a
    };
    vec![
        (
            "current",
            "current: 1024-sample window, linear to 4 kHz then log",
            sim(Analysis::current()),
        ),
        (
            "A",
            "A: 1024-sample window, log bars, interpolated between bins",
            sim(Analysis::log_1024()),
        ),
        (
            "B",
            "B: 1024-sample window zero-padded to 4096, log bars",
            sim(Analysis::log_1024_padded()),
        ),
        (
            "C",
            "C: 4096-sample window, log bars",
            sim(Analysis::log_4096()),
        ),
        (
            "D",
            "D: 4096-sample window below ~1 kHz, 1024 above, log bars",
            sim(Analysis::log_dual()),
        ),
    ]
}

// ---------- synthetic signals ----------

fn samples(secs: f32) -> usize {
    (secs * RATE) as usize
}

fn silence(secs: f32) -> Vec<f32> {
    vec![0.0; samples(secs)]
}

fn tone(freq: f32, amp: f32, secs: f32) -> Vec<f32> {
    (0..samples(secs))
        .map(|n| amp * (2.0 * std::f32::consts::PI * freq * n as f32 / RATE).sin())
        .collect()
}

fn add(into: &mut [f32], at: usize, part: &[f32]) {
    for (dst, src) in into.iter_mut().skip(at).zip(part) {
        *dst += src;
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    }
}

/// Pink noise (Paul Kellet's economy filter), deterministic.
fn pink(secs: f32, amp: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng(seed);
    let (mut b0, mut b1, mut b2) = (0.0_f32, 0.0_f32, 0.0_f32);
    (0..samples(secs))
        .map(|_| {
            let white = rng.next();
            b0 = 0.99765 * b0 + white * 0.099_046;
            b1 = 0.96300 * b1 + white * 0.296_516_4;
            b2 = 0.57000 * b2 + white * 1.052_691_3;
            amp * (b0 + b1 + b2 + white * 0.1848) * 0.2
        })
        .collect()
}

/// A bar of something music-shaped: kick, bass line with harmonics, a chord,
/// hi-hats and a quiet pink bed.
fn music(secs: f32) -> Vec<f32> {
    let n = samples(secs);
    let mut out = pink(secs, 0.25, 7);
    let mut rng = Rng(99);
    let beat = samples(0.5);
    for (i, start) in (0..n).step_by(beat).enumerate() {
        // kick: 55 Hz with a fast pitch drop and an 90 ms decay
        let kick: Vec<f32> = (0..samples(0.35))
            .map(|k| {
                let t = k as f32 / RATE;
                let f = 55.0 + 60.0 * (-t / 0.03).exp();
                0.9 * (-t / 0.09).exp() * (2.0 * std::f32::consts::PI * f * t).sin()
            })
            .collect();
        add(&mut out, start, &kick);
        // bass note with two harmonics
        let root = [82.41_f32, 98.0, 110.0, 73.42][i % 4];
        for (h, a) in [(1.0_f32, 0.35_f32), (2.0, 0.15), (3.0, 0.08)] {
            add(&mut out, start, &tone(root * h, a, 0.45));
        }
        // off-beat hi-hat: short high-passed noise burst
        let mut prev = 0.0_f32;
        let hat: Vec<f32> = (0..samples(0.06))
            .map(|k| {
                let white = rng.next();
                let hp = white - prev;
                prev = white;
                0.25 * (-(k as f32 / RATE) / 0.02).exp() * hp
            })
            .collect();
        add(&mut out, start + beat / 2, &hat);
    }
    // sustained chord (A minor-ish) with one harmonic each
    for f in [220.0_f32, 261.63, 329.63, 440.0] {
        add(&mut out, 0, &tone(f, 0.07, secs));
        add(&mut out, 0, &tone(f * 2.0, 0.03, secs));
    }
    out
}

// ---------- running a variant ----------

struct Run {
    /// Fresh bands per hop, before decay; hop `k` has seen `(k + 1) * HOP` signal samples.
    frames: Vec<[f32; NUM_BANDS]>,
    bands: Arc<Mutex<VisBands>>,
}

fn run(analysis: Analysis, pcm: &[f32]) -> Run {
    let bands = Arc::new(Mutex::new(VisBands::new()));
    let mut processor = BandProcessor::with_analysis(Arc::clone(&bands), RATE, analysis);
    // Pre-roll one full window of silence so every later hop ends on the newest sample.
    let needed = analysis.window.max(analysis.low.map_or(0, |l| l.window));
    processor.push_mono_samples(vec![0.0; needed]);
    let mut frames = Vec::with_capacity(pcm.len() / HOP);
    for chunk in pcm.chunks_exact(HOP) {
        let before = processor.hops();
        processor.push_mono_samples(chunk.iter().copied());
        assert_eq!(processor.hops(), before + 1, "one hop per pushed chunk");
        frames.push(processor.last_frame());
    }
    Run { frames, bands }
}

fn bar_position(freq: f32, axis: AxisScale) -> f32 {
    freq_to_x_fraction(freq, RATE, axis) * NUM_BANDS as f32
}

fn around(frame: &[f32; NUM_BANDS], idx: usize) -> f32 {
    let lo = idx.saturating_sub(1);
    let hi = (idx + 1).min(NUM_BANDS - 1);
    frame[lo..=hi].iter().copied().fold(0.0, f32::max)
}

fn db(a: f32, b: f32) -> f32 {
    20.0 * (a.max(1e-12) / b.max(1e-12)).log10()
}

fn argmax(frame: &[f32; NUM_BANDS]) -> usize {
    frame
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i)
}

/// Signed error, in bars, between where a steady tone peaks and where the axis puts it.
fn placement_error(analysis: Analysis, freq: f32) -> f32 {
    let r = run(analysis, &tone(freq, 0.5, 0.4));
    let peak = argmax(r.frames.last().unwrap());
    (peak as f32 + 0.5) - bar_position(freq, analysis.axis)
}

/// Depth in dB of the valley between two simultaneous tones, as (worst hop,
/// median hop) over the steady part. Two tones closer than the window can
/// separate beat against each other, so a single frame can show a deep valley
/// that is gone a few milliseconds later; the worst hop is the honest number.
fn valley_db(analysis: Analysis, pcm: &[f32], f1: f32, f2: f32) -> (f32, f32) {
    let r = run(analysis, pcm);
    let i1 = bar_position(f1, analysis.axis) as usize;
    let i2 = (bar_position(f2, analysis.axis) as usize).min(NUM_BANDS - 1);
    if i2 < i1 + 2 {
        return (0.0, 0.0);
    }
    let steady_from = samples(0.3) / HOP;
    let mut depths: Vec<f32> = r.frames[steady_from..]
        .iter()
        .map(|frame| {
            let valley = frame[i1..=i2].iter().copied().fold(f32::MAX, f32::min);
            db(around(frame, i1).min(around(frame, i2)), valley)
        })
        .collect();
    depths.sort_by(f32::total_cmp);
    (depths[0], depths[depths.len() / 2])
}

fn two_tones(f1: f32, f2: f32) -> Vec<f32> {
    let mut pcm = tone(f1, 0.4, 0.6);
    add(&mut pcm, 0, &tone(f2, 0.4, 0.6));
    pcm
}

/// Bars within 6 dB of the peak for a single steady tone.
fn width_bars(analysis: Analysis, freq: f32) -> usize {
    let r = run(analysis, &tone(freq, 0.5, 0.6));
    let frame = r.frames.last().unwrap();
    let peak = frame.iter().copied().fold(0.0, f32::max);
    frame.iter().filter(|&&v| v >= 0.5 * peak).count()
}

/// (ms until the tone's bar reaches half its steady value, ms until it falls to a tenth after the tone stops).
fn onset_release_ms(analysis: Analysis, freq: f32) -> (f32, f32) {
    let (lead, hold, tail) = (0.2_f32, 0.6_f32, 0.5_f32);
    let mut pcm = silence(lead + hold + tail);
    add(&mut pcm, samples(lead), &tone(freq, 0.5, hold));
    let r = run(analysis, &pcm);
    let idx = bar_position(freq, analysis.axis) as usize;
    let seen = |hop: usize| ((hop + 1) * HOP) as f32;
    let on = samples(lead) as f32;
    let off = samples(lead + hold) as f32;
    let steady_hop = (off as usize / HOP).saturating_sub(2);
    let steady = around(&r.frames[steady_hop], idx);
    let onset = r
        .frames
        .iter()
        .enumerate()
        .find(|(hop, frame)| seen(*hop) > on && around(frame, idx) >= 0.5 * steady)
        .map_or(f32::NAN, |(hop, _)| (seen(hop) - on) / RATE * 1000.0);
    let release = r
        .frames
        .iter()
        .enumerate()
        .find(|(hop, frame)| seen(*hop) > off && around(frame, idx) <= 0.1 * steady)
        .map_or(f32::NAN, |(hop, _)| (seen(hop) - off) / RATE * 1000.0);
    (onset, release)
}

/// Level of 1.2..2 kHz relative to 350..600 Hz on pink noise, in dB.
fn pink_tilt_db(analysis: Analysis) -> f32 {
    let r = run(analysis, &pink(3.0, 0.5, 1234));
    let skip = samples(0.5) / HOP;
    let mean_in = |lo_hz: f32, hi_hz: f32| {
        let lo = bar_position(lo_hz, analysis.axis) as usize;
        let hi = (bar_position(hi_hz, analysis.axis) as usize).min(NUM_BANDS - 1);
        let mut sum = 0.0_f64;
        let mut count = 0_u64;
        for frame in &r.frames[skip..] {
            for v in &frame[lo..=hi] {
                sum += f64::from(*v) * f64::from(*v);
                count += 1;
            }
        }
        (sum / count as f64).sqrt() as f32
    };
    db(mean_in(1200.0, 2000.0), mean_in(350.0, 600.0))
}

/// Median level, in dB, of `probe` relative to a 100 Hz tone mixed into the
/// same signal. Measurement only, added after the first results to put a
/// number on what the frames show: how a longer window shifts everything that
/// is not a resolved tone against the bass.
fn level_vs_100hz_db(
    analysis: Analysis,
    pcm: &[f32],
    probe: impl Fn(&[f32; NUM_BANDS]) -> f32,
) -> f32 {
    let r = run(analysis, pcm);
    let reference = bar_position(100.0, analysis.axis) as usize;
    let steady_from = samples(0.3) / HOP;
    let mut levels: Vec<f32> = r.frames[steady_from..]
        .iter()
        .map(|frame| db(probe(frame), around(frame, reference)))
        .collect();
    levels.sort_by(f32::total_cmp);
    levels[levels.len() / 2]
}

/// Level of an equal-amplitude tone at `freq` against the 100 Hz one.
fn tone_level_db(analysis: Analysis, freq: f32) -> f32 {
    let idx = (bar_position(freq, analysis.axis) as usize).min(NUM_BANDS - 1);
    level_vs_100hz_db(analysis, &two_tones(100.0, freq), |frame| {
        around(frame, idx)
    })
}

/// RMS bar level of a pink bed between `lo_hz` and `hi_hz` against a 100 Hz tone.
fn pink_level_db(analysis: Analysis, lo_hz: f32, hi_hz: f32) -> f32 {
    let mut pcm = pink(3.0, 0.25, 4321);
    add(&mut pcm, 0, &tone(100.0, 0.4, 3.0));
    let lo = bar_position(lo_hz, analysis.axis) as usize;
    let hi = (bar_position(hi_hz, analysis.axis) as usize).min(NUM_BANDS - 1);
    level_vs_100hz_db(analysis, &pcm, |frame| {
        let bars = &frame[lo..=hi];
        (bars.iter().map(|v| v * v).sum::<f32>() / bars.len() as f32).sqrt()
    })
}

fn micros_per_hop(analysis: Analysis, pcm: &[f32]) -> f32 {
    let bands = Arc::new(Mutex::new(VisBands::new()));
    let mut processor = BandProcessor::with_analysis(bands, RATE, analysis);
    let start = Instant::now();
    for chunk in pcm.chunks_exact(HOP) {
        processor.push_mono_samples(chunk.iter().copied());
    }
    start.elapsed().as_secs_f32() * 1e6 / processor.hops().max(1) as f32
}

/// Cost per hop in microseconds as (best, median) over interleaved rounds.
/// One wall-clock pass per variant moved by almost a factor of two between two
/// runs on a busy host, so the best round is the estimate of the work itself
/// and the median shows what the host's load adds.
fn cost_rounds(all: &[(&str, &str, Analysis)], pcm: &[f32], rounds: usize) -> Vec<(f32, f32)> {
    let mut timings = vec![Vec::with_capacity(rounds); all.len()];
    for _ in 0..rounds {
        for (slot, (_, _, analysis)) in timings.iter_mut().zip(all) {
            slot.push(micros_per_hop(*analysis, pcm));
        }
    }
    timings
        .into_iter()
        .map(|mut t| {
            t.sort_by(f32::total_cmp);
            (t[0], t[t.len() / 2])
        })
        .collect()
}

// ---------- rendering ----------

fn test_state(out: &std::path::Path) -> SharedState {
    let config_dir = out.join("config");
    let cache_dir = out.join("cache");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();
    let mut configs = config::Configs::new(&config_dir, &cache_dir).unwrap();
    config::apply_config_override(
        &mut configs.app_config,
        "enable_audio_visualization",
        "true",
    )
    .unwrap();
    config::set_config(configs);
    Arc::new(State::new(false, Arc::new(Mutex::new(VecDeque::new()))))
}

/// One image: every variant stacked, each drawn by the real renderer.
fn render_scene(state: &SharedState, name: &str, pcm: &[f32], width: u16, out: &std::path::Path) {
    let all = variants();
    let rows = all.len() as u16 * (PANEL_ROWS + 2);
    let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
    let theme = state.ui.lock().theme.clone();
    let runs: Vec<Run> = all.iter().map(|(_, _, a)| run(*a, pcm)).collect();
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame.render_widget(ratatui::widgets::Block::default().style(theme.app()), area);
            for (i, ((_, label, _), r)) in all.iter().zip(&runs).enumerate() {
                let y = i as u16 * (PANEL_ROWS + 2);
                frame.render_widget(Paragraph::new(*label), Rect::new(1, y, width - 1, 1));
                {
                    let src = r.bands.lock();
                    let mut dst = state.vis_bands.as_ref().unwrap().lock();
                    dst.values = src.values;
                    dst.peak_envelope = src.peak_envelope;
                    dst.sample_rate = src.sample_rate;
                    dst.axis = src.axis;
                    dst.is_active = true;
                    dst.updated_at = Instant::now();
                }
                super::streaming::render_audio_visualization(
                    frame,
                    state,
                    &theme,
                    Rect::new(0, y + 1, width, PANEL_ROWS),
                );
            }
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut cells = Vec::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            cells.push(serde_json::json!({
                "s": cell.symbol(),
                "fg": format!("{:?}", cell.fg),
                "bg": format!("{:?}", cell.bg),
                "m": cell.modifier.bits(),
            }));
        }
    }
    let doc = serde_json::json!({"name": name, "width": buf.area.width, "height": buf.area.height, "cells": cells});
    std::fs::write(out.join(format!("{name}-{width}.json")), doc.to_string()).unwrap();
}

#[test]
#[ignore = "prototype harness; needs PROTO_OUT"]
fn vis_proto() {
    let out = std::path::PathBuf::from(std::env::var("PROTO_OUT").expect("PROTO_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let all = variants();

    // ---- harness self-test: one case that must pass, two that must not ----
    for (id, _, analysis) in &all {
        let err = placement_error(*analysis, 1_000.0);
        assert!(
            err.abs() <= 1.0,
            "self-test: 1 kHz is {err} bars off in {id}"
        );
        let quiet = run(*analysis, &silence(0.3));
        assert!(
            quiet.frames.iter().all(|f| f.iter().all(|v| *v == 0.0)),
            "self-test: silence must give zero bars in {id}"
        );
        // A single tone has no second peak, so the resolution oracle must say "not resolved".
        let (lone, _) = valley_db(*analysis, &tone(100.0, 0.4, 0.6), 100.0, 150.0);
        assert!(
            lone < 6.0,
            "self-test: lone 100 Hz tone reads as resolved ({lone} dB) in {id}"
        );
    }

    // ---- declared measurements, identical for every variant ----
    let costs = cost_rounds(&all, &music(5.0), 15);
    let mut rows = Vec::new();
    for ((id, label, analysis), (cost_best, cost_median)) in all.iter().zip(&costs) {
        let placement: Vec<(f32, f32)> = [100.0_f32, 440.0, 1_000.0, 5_000.0, 10_000.0]
            .iter()
            .map(|&f| (f, placement_error(*analysis, f)))
            .collect();
        let (on_60, off_60) = onset_release_ms(*analysis, 60.0);
        let (on_1k, off_1k) = onset_release_ms(*analysis, 1_000.0);
        let (on_5k, off_5k) = onset_release_ms(*analysis, 5_000.0);
        rows.push(serde_json::json!({
            "id": id,
            "label": label,
            "placement_error_bars": placement.iter().map(|(f, e)| serde_json::json!({"hz": f, "bars": e})).collect::<Vec<_>>(),
            "valley_db_100_150_worst_median": valley_db(*analysis, &two_tones(100.0, 150.0), 100.0, 150.0),
            "valley_db_60_80_worst_median": valley_db(*analysis, &two_tones(60.0, 80.0), 60.0, 80.0),
            "valley_db_110_220_worst_median": valley_db(*analysis, &two_tones(110.0, 220.0), 110.0, 220.0),
            "width_bars_100hz": width_bars(*analysis, 100.0),
            "width_bars_1khz": width_bars(*analysis, 1_000.0),
            "onset_ms": {"60": on_60, "1000": on_1k, "5000": on_5k},
            "release_ms": {"60": off_60, "1000": off_1k, "5000": off_5k},
            "pink_tilt_db_1k2_2k_vs_350_600": pink_tilt_db(*analysis),
            "level_vs_100hz_tone_db": {
                "tone_1k": tone_level_db(*analysis, 1_000.0),
                "tone_4k": tone_level_db(*analysis, 4_000.0),
                "tone_10k": tone_level_db(*analysis, 10_000.0),
                "pink_150_600": pink_level_db(*analysis, 150.0, 600.0),
                "pink_2k_8k": pink_level_db(*analysis, 2_000.0, 8_000.0),
            },
            "micros_per_hop_best_of_15": cost_best,
            "micros_per_hop_median_of_15": cost_median,
            "tick_fraction": {
                "100": freq_to_x_fraction(100.0, RATE, analysis.axis),
                "1000": freq_to_x_fraction(1_000.0, RATE, analysis.axis),
                "10000": freq_to_x_fraction(10_000.0, RATE, analysis.axis),
            },
        }));
    }
    let report = serde_json::json!({
        "sample_rate": RATE,
        "hop": HOP,
        "hops_per_second": RATE / HOP as f32,
        "release_build": !cfg!(debug_assertions),
        "variants": rows,
    });
    let name = if cfg!(debug_assertions) {
        "metrics-debug.json"
    } else {
        "metrics-release.json"
    };
    std::fs::write(
        out.join(name),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();

    // ---- frames for the human gate ----
    let state = test_state(&out);
    let mut octaves = silence(0.6);
    for k in 0..9 {
        add(&mut octaves, 0, &tone(62.5 * 2.0_f32.powi(k), 0.1, 0.6));
    }
    let bass_pair = two_tones(100.0, 150.0);
    let song = music(2.02);
    for width in [84_u16, 150] {
        render_scene(&state, "octave-tones", &octaves, width, &out);
        render_scene(&state, "bass-pair-100-150", &bass_pair, width, &out);
        render_scene(&state, "music-like", &song, width, &out);
    }
}
