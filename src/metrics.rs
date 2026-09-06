//! Measurement helpers for the correctness and quality gates.
//!
//! These separate three things the docs insist on separating: correctness of
//! length and clock, perceptual quality, and cost. Nothing here returns a
//! single "quality score", and the pitch estimator used for grading is not the
//! one an engine uses internally (validation.md sec.5).

use crate::audio::AudioBuffer;
use serde::{Deserialize, Serialize};

/// RMS difference between two buffers, in dBFS. `-inf` for an exact match.
pub fn rms_difference_db(a: &AudioBuffer, b: &AudioBuffer) -> f64 {
    let n = a.frames().min(b.frames());
    let ch = a.channel_count().min(b.channel_count());
    if n == 0 || ch == 0 {
        return f64::NEG_INFINITY;
    }
    let mut acc = 0.0f64;
    for c in 0..ch {
        for i in 0..n {
            let d = (a.channel(c)[i] - b.channel(c)[i]) as f64;
            acc += d * d;
        }
    }
    let rms = (acc / (n * ch) as f64).sqrt();
    if rms == 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * rms.log10()
    }
}

pub fn is_bit_identical(a: &AudioBuffer, b: &AudioBuffer) -> bool {
    if a.frames() != b.frames() || a.channel_count() != b.channel_count() {
        return false;
    }
    for c in 0..a.channel_count() {
        if a.channel(c) != b.channel(c) {
            return false;
        }
    }
    true
}

pub fn any_non_finite(a: &AudioBuffer) -> bool {
    a.planes().iter().any(|p| p.iter().any(|v| !v.is_finite()))
}

/// Frequency of a steady tone by parabolic interpolation on the magnitude
/// spectrum of a Hann-windowed segment. Deliberately a different method from
/// the YIN estimator in `analysis`, so the engine is not marking its own work.
pub fn dominant_hz(buf: &AudioBuffer, sample_rate: u32, start: usize, len: usize) -> Option<f64> {
    let n = len.next_power_of_two().min(1 << 16);
    if buf.frames() < start + n {
        return None;
    }
    let mut stft = crate::dsp::stft::RealStft::new(n);
    let window = crate::dsp::window::hann_periodic(n);
    let mono = buf.mono_sum();
    let mut time: Vec<f32> = (0..n).map(|i| mono[start + i] * window[i]).collect();
    let mut spec = vec![realfft::num_complex::Complex32::new(0.0, 0.0); n / 2 + 1];
    stft.forward(&mut time, &mut spec);
    let mags: Vec<f32> = spec.iter().map(|c| c.norm()).collect();
    let (mut peak, mut peak_v) = (0usize, 0.0f32);
    for (k, m) in mags.iter().enumerate().take(mags.len() - 1).skip(1) {
        if *m > peak_v {
            peak_v = *m;
            peak = k;
        }
    }
    if peak == 0 || peak_v <= 0.0 {
        return None;
    }
    let (a, b, c) = (
        mags[peak - 1].max(1e-12).ln(),
        mags[peak].max(1e-12).ln(),
        mags[peak + 1].max(1e-12).ln(),
    );
    // p = 0.5*(a-c)/(a-2b+c) on log magnitudes: the sign matters, and getting
    // it backwards reads as a real pitch error of tens of cents.
    let denom = a - 2.0 * b + c;
    let delta = if denom.abs() > 1e-9 { 0.5 * (a - c) / denom } else { 0.0 };
    Some((peak as f64 + delta as f64) * sample_rate as f64 / n as f64)
}

/// Pitch error in cents against a known frequency.
pub fn pitch_error_cents(measured_hz: f64, expected_hz: f64) -> f64 {
    1200.0 * (measured_hz / expected_hz).log2()
}

/// Positions of impulse-like peaks: any sample above `rel` times the global
/// peak that also dominates a short neighbourhood.
pub fn peak_positions(buf: &AudioBuffer, rel: f32, min_gap: usize) -> Vec<usize> {
    let mono = buf.mono_sum();
    let peak = mono.iter().fold(0.0f32, |a, v| a.max(v.abs()));
    if peak <= 0.0 {
        return Vec::new();
    }
    let thr = peak * rel;
    let mut out: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < mono.len() {
        if mono[i].abs() >= thr {
            let end = (i + min_gap).min(mono.len());
            let mut best = i;
            for j in i..end {
                if mono[j].abs() > mono[best].abs() {
                    best = j;
                }
            }
            out.push(best);
            i = best + min_gap;
        } else {
            i += 1;
        }
    }
    out
}

/// Onset timing error in milliseconds between expected and measured positions,
/// matched in order. Reported per hit; the docs ask for the distribution, not
/// one number.
pub fn onset_errors_ms(expected: &[usize], measured: &[usize], sample_rate: u32) -> Vec<f64> {
    let n = expected.len().min(measured.len());
    (0..n)
        .map(|i| (measured[i] as f64 - expected[i] as f64) * 1000.0 / sample_rate as f64)
        .collect()
}

/// Inter-channel correlation, for the stereo fixtures. `1` means identical,
/// `-1` means inverted.
pub fn channel_correlation(buf: &AudioBuffer) -> Option<f64> {
    if buf.channel_count() < 2 || buf.frames() == 0 {
        return None;
    }
    let (a, b) = (buf.channel(0), buf.channel(1));
    let mut num = 0.0f64;
    let mut ea = 0.0f64;
    let mut eb = 0.0f64;
    for i in 0..buf.frames() {
        let (x, y) = (a[i] as f64, b[i] as f64);
        num += x * y;
        ea += x * x;
        eb += y * y;
    }
    if ea <= 0.0 || eb <= 0.0 {
        return None;
    }
    Some(num / (ea.sqrt() * eb.sqrt()))
}

/// Short-time envelope in dB, one value per hop.
pub fn envelope_db(buf: &AudioBuffer, win: usize, hop: usize) -> Vec<f64> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + win <= buf.frames() {
        let mut acc = 0.0f64;
        for c in 0..buf.channel_count() {
            for v in &buf.channel(c)[at..at + win] {
                acc += (*v as f64) * (*v as f64);
            }
        }
        let rms = (acc / (win * buf.channel_count()) as f64).sqrt();
        out.push(if rms > 1e-12 { 20.0 * rms.log10() } else { -240.0 });
        at += hop;
    }
    out
}

/// How far a decaying envelope wanders from the straight line it should be.
///
/// A reverb tail decays linearly in dB, so a least-squares line through the
/// log envelope is the right reference and the residual is what the engine
/// added. Reported in dB rms, which is directly the roughness you hear.
pub fn tail_roughness_db(buf: &AudioBuffer, sample_rate: u32, skip_s: f64, take_s: f64) -> f64 {
    let win = 2048usize;
    let hop = 512usize;
    let env = envelope_db(buf, win, hop);
    let a = ((skip_s * sample_rate as f64) as usize / hop).min(env.len());
    let b = (a + (take_s * sample_rate as f64) as usize / hop).min(env.len());
    if b <= a + 4 {
        return 0.0;
    }
    let seg = &env[a..b];
    let n = seg.len() as f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for (i, v) in seg.iter().enumerate() {
        let x = i as f64;
        sx += x;
        sy += v;
        sxx += x * x;
        sxy += x * v;
    }
    let denom = n * sxx - sx * sx;
    if denom.abs() < 1e-12 {
        return 0.0;
    }
    let slope = (n * sxy - sx * sy) / denom;
    let intercept = (sy - slope * sx) / n;
    let mut acc = 0.0;
    for (i, v) in seg.iter().enumerate() {
        let fit = intercept + slope * i as f64;
        acc += (v - fit) * (v - fit);
    }
    (acc / n).sqrt()
}

/// 10%-to-90% rise time of the envelope around a peak, in milliseconds.
///
/// This is the number that says whether an attack survived: a phase vocoder
/// that smears one turns a 1 ms rise into tens of milliseconds.
pub fn attack_rise_ms(buf: &AudioBuffer, peak_at: usize, sample_rate: u32) -> Option<f64> {
    let mono = buf.mono_sum();
    let look = (sample_rate as usize / 20).min(peak_at);
    let from = peak_at - look;
    let to = (peak_at + sample_rate as usize / 100).min(mono.len());
    if to <= from + 2 {
        return None;
    }
    // Rectified, lightly smoothed envelope.
    let w = 16usize;
    let env: Vec<f32> = (from..to)
        .map(|i| {
            let a = i.saturating_sub(w);
            let b = (i + w).min(mono.len());
            mono[a..b].iter().fold(0.0f32, |m, v| m.max(v.abs()))
        })
        .collect();
    let peak = env.iter().cloned().fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return None;
    }
    let hi_i = env.iter().position(|v| *v >= peak * 0.9)?;
    let lo_i = env[..=hi_i].iter().rposition(|v| *v <= peak * 0.1).unwrap_or(0);
    Some((hi_i - lo_i) as f64 * 1000.0 / sample_rate as f64)
}

/// Energy just before an attack, relative to the attack itself, in dB.
///
/// Pre-echo: a spectral engine spreads a transient backwards across its window,
/// and that leading smear is what makes a stretched drum sound soft.
pub fn pre_attack_db(buf: &AudioBuffer, peak_at: usize, sample_rate: u32) -> Option<f64> {
    let mono = buf.mono_sum();
    let span = sample_rate as usize / 50; // 20 ms
    if peak_at < span * 2 || peak_at + span >= mono.len() {
        return None;
    }
    let before: f64 = mono[peak_at - span * 2..peak_at - span / 2]
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum();
    let at: f64 = mono[peak_at..peak_at + span]
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum();
    if at <= 0.0 {
        return None;
    }
    Some(10.0 * (before / at).max(1e-12).log10())
}

/// Inter-channel delay in frames, by cross-correlation over a window.
///
/// A pair of microphones on one source differs by a fixed delay, and that delay
/// *is* the stereo image. An engine that decides per channel moves it.
pub fn interchannel_lag(
    buf: &AudioBuffer,
    start: usize,
    len: usize,
    max_lag: usize,
) -> Option<i64> {
    if buf.channel_count() < 2 || start + len + max_lag >= buf.frames() {
        return None;
    }
    let (a, b) = (buf.channel(0), buf.channel(1));
    let mut best = 0i64;
    let mut best_score = f64::NEG_INFINITY;
    for lag in -(max_lag as i64)..=(max_lag as i64) {
        let mut num = 0.0f64;
        let mut ea = 0.0f64;
        let mut eb = 0.0f64;
        for i in 0..len {
            let ia = start + i;
            let ib = (start as i64 + i as i64 + lag) as usize;
            if ib >= b.len() {
                continue;
            }
            let (x, y) = (a[ia] as f64, b[ib] as f64);
            num += x * y;
            ea += x * x;
            eb += y * y;
        }
        if ea <= 0.0 || eb <= 0.0 {
            continue;
        }
        let score = num / (ea.sqrt() * eb.sqrt());
        if score > best_score {
            best_score = score;
            best = lag;
        }
    }
    if best_score == f64::NEG_INFINITY {
        None
    } else {
        Some(best)
    }
}

/// Two-pole lowpass, for isolating the band a measurement is about.
///
/// Zero-phase is not needed here: every measurement built on this compares a
/// filtered signal against another filtered the same way.
pub fn lowpass(buf: &AudioBuffer, cutoff_hz: f64, sample_rate: u32) -> AudioBuffer {
    let w = (std::f64::consts::TAU * cutoff_hz / sample_rate as f64).tan();
    let a = w / (1.0 + w);
    let planes: Vec<Vec<f32>> = (0..buf.channel_count())
        .map(|c| {
            let mut y1 = 0.0f64;
            let mut y2 = 0.0f64;
            buf.channel(c)
                .iter()
                .map(|v| {
                    y1 += a * (*v as f64 - y1);
                    y2 += a * (y1 - y2);
                    y2 as f32
                })
                .collect()
        })
        .collect();
    AudioBuffer::from_planar(planes).expect("uniform planes")
}

/// How rough the low end is, in dB rms about its own mean.
///
/// A sustained bass note has a flat envelope; a phase discontinuity in it shows
/// up here as a spike the ear hears as the low end cracking.
pub fn bass_roughness_db(buf: &AudioBuffer, sample_rate: u32, cutoff_hz: f64) -> f64 {
    let lp = lowpass(buf, cutoff_hz, sample_rate);
    let env = envelope_db(&lp, 2048, 512);
    if env.len() < 12 {
        return 0.0;
    }
    let a = env.len() / 6;
    let b = env.len() - env.len() / 6;
    let m = env[a..b].iter().sum::<f64>() / (b - a) as f64;
    (env[a..b].iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (b - a) as f64).sqrt()
}

/// Peak and RMS of the render, reported instead of normalising it away
/// (system-design.md sec.12).
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct LevelReport {
    pub peak: f32,
    pub rms_dbfs: f64,
    pub headroom_db: f64,
}

pub fn level(buf: &AudioBuffer) -> LevelReport {
    let peak = buf.peak();
    let mut acc = 0.0f64;
    let mut n = 0usize;
    for p in buf.planes() {
        for v in p {
            acc += (*v as f64) * (*v as f64);
            n += 1;
        }
    }
    let rms = if n == 0 { 0.0 } else { (acc / n as f64).sqrt() };
    LevelReport {
        peak,
        rms_dbfs: if rms > 0.0 { 20.0 * rms.log10() } else { f64::NEG_INFINITY },
        headroom_db: if peak > 0.0 { -20.0 * (peak as f64).log10() } else { f64::INFINITY },
    }
}

/// The reusable report row from validation.md sec.8. `null` means not measured,
/// never zero.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestReport {
    pub status: String,
    pub engine: String,
    pub engine_revision: Option<u32>,
    pub fixture_hash: Option<String>,
    pub sample_rate: u32,
    pub channels: usize,
    pub alpha: f64,
    pub pitch_semitones: f64,
    pub expected_frames: Option<u64>,
    pub actual_frames: Option<u64>,
    pub pitch_error_cents: Option<f64>,
    pub onset_error_ms: Option<f64>,
    pub callback_p99_ms: Option<f64>,
    pub underruns: Option<u64>,
    pub listening_notes: Option<String>,
}

impl TestReport {
    pub fn not_run(engine: &str, sample_rate: u32, channels: usize, alpha: f64) -> Self {
        Self {
            status: "not_run".to_string(),
            engine: engine.to_string(),
            engine_revision: None,
            fixture_hash: None,
            sample_rate,
            channels,
            alpha,
            pitch_semitones: 0.0,
            expected_frames: None,
            actual_frames: None,
            pitch_error_cents: None,
            onset_error_ms: None,
            callback_p99_ms: None,
            underruns: None,
            listening_notes: None,
        }
    }
}
