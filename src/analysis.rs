//! Offline analysis: onsets, F0 with confidence, voicing and a content
//! classification for `Auto`.
//!
//! Everything here runs on a worker, never in the audio callback, and results
//! carry an analyzer version so a cache entry can be invalidated when the
//! analyzer changes (system-design.md sec.10).
//!
//! Detection output is a *hint*. It becomes a hard timing requirement only when
//! the user promotes it to an anchor (system-design.md sec.5).

use crate::audio::{AudioBuffer, Fnv1a128, SourceIdentity};
use crate::document::{DetectedNote, EngineMode, NoteId};
use crate::dsp::stft::RealStft;
use crate::dsp::window::hann_periodic;
use realfft::num_complex::Complex32;
use serde::{Deserialize, Serialize};

/// Bump when anything here changes what it produces.
pub const ANALYZER_VERSION: u32 = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Onset {
    pub frame: u64,
    /// Detection-function strength at the peak, relative to the local median.
    pub strength: f32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClass {
    Silence,
    Percussive,
    MonophonicTonal,
    PolyphonicTonal,
    Mixed,
}

impl ContentClass {
    /// The concrete mode `Auto` compiles to. The user can always override, and
    /// the choice is recorded in the plan so a render repeats (sec.6).
    pub fn suggested_mode(self) -> EngineMode {
        match self {
            ContentClass::Silence => EngineMode::ElasticPro,
            ContentClass::Percussive => EngineMode::Rhythmic,
            ContentClass::MonophonicTonal => EngineMode::Soloist,
            ContentClass::PolyphonicTonal => EngineMode::ElasticPro,
            ContentClass::Mixed => EngineMode::ElasticPro,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ContentClass::Silence => "silence",
            ContentClass::Percussive => "percussive",
            ContentClass::MonophonicTonal => "monophonic tonal",
            ContentClass::PolyphonicTonal => "polyphonic tonal",
            ContentClass::Mixed => "mixed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Analysis {
    pub analyzer_version: u32,
    pub source_id: String,
    pub sample_rate: u32,
    pub frames: u64,
    pub hop: usize,
    pub onsets: Vec<Onset>,
    /// One entry per analysis hop.
    pub f0_hz: Vec<f32>,
    pub f0_confidence: Vec<f32>,
    pub voiced: Vec<bool>,
    pub onset_strength: Vec<f32>,
    pub rms: Vec<f32>,
    pub class: ContentClass,
    /// How much of the energy sits in transient-like frames, 0..1.
    pub percussivity: f32,
    /// Spectral flatness averaged over voiced frames, 0..1.
    pub tonality: f32,
    pub notes: Vec<DetectedNote>,
}

impl Analysis {
    /// `analysis_key = hash(source_content, rate, layout, analyzer_version,
    /// settings)`. Path and mtime are deliberately not part of it.
    pub fn key(source: &SourceIdentity, settings: &AnalysisSettings) -> String {
        let mut h = Fnv1a128::new();
        h.write_str(&source.id);
        h.write_u64(source.sample_rate as u64);
        h.write_u64(source.channels as u64);
        h.write_u64(ANALYZER_VERSION as u64);
        h.write_u64(settings.fft_size as u64);
        h.write_u64(settings.hop as u64);
        h.write_f64(settings.onset_threshold as f64);
        h.write_f64(settings.f0_min as f64);
        h.write_f64(settings.f0_max as f64);
        h.finish_hex()
    }

    pub fn time_of(&self, index: usize) -> u64 {
        (index * self.hop) as u64
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AnalysisSettings {
    pub fft_size: usize,
    pub hop: usize,
    /// Peak must exceed the local median by this many median-absolute
    /// deviations.
    pub onset_threshold: f32,
    pub f0_min: f32,
    pub f0_max: f32,
    /// Minimum spacing between accepted onsets, in seconds.
    pub min_onset_spacing: f32,
}

impl Default for AnalysisSettings {
    fn default() -> Self {
        Self {
            fft_size: 2048,
            hop: 256,
            onset_threshold: 3.0,
            // Low E on a bass guitar is 41 Hz and a synth sub goes lower. A
            // 55 Hz floor put the whole bottom octave outside detection, which
            // is also the octave the specialised low path exists for: with no
            // note found down there the compiler could not tell there was any
            // bass to help.
            f0_min: 35.0,
            f0_max: 1200.0,
            min_onset_spacing: 0.030,
        }
    }
}

pub fn analyze(
    buf: &AudioBuffer,
    sample_rate: u32,
    source_id: &str,
    settings: &AnalysisSettings,
) -> Analysis {
    let n = settings.fft_size;
    let hop = settings.hop;
    let mono = buf.mono_sum();
    let frames = buf.frames();
    let steps = if frames < n { 0 } else { (frames - n) / hop + 1 };

    let mut stft = RealStft::new(n);
    let window = hann_periodic(n);
    let bins = n / 2 + 1;
    let mut spec = vec![Complex32::new(0.0, 0.0); bins];
    let mut mag = vec![0.0f32; bins];
    let mut prev_mag = vec![0.0f32; bins];
    let mut time = vec![0.0f32; n];

    let mut flux = Vec::with_capacity(steps);
    let mut flux_share = Vec::with_capacity(steps);
    let mut rms = Vec::with_capacity(steps);
    let mut flatness = Vec::with_capacity(steps);

    for m in 0..steps {
        let start = m * hop;
        for i in 0..n {
            time[i] = mono[start + i] * window[i];
        }
        stft.forward(&time, &mut spec);
        let mut e = 0.0f64;
        let mut log_sum = 0.0f64;
        let mut lin_sum = 0.0f64;
        let mut f = 0.0f32;
        for k in 0..bins {
            mag[k] = spec[k].norm();
            // Half-wave rectified flux: only growth counts as an onset.
            let d = mag[k] - prev_mag[k];
            if d > 0.0 {
                f += d;
            }
            let p = (mag[k] as f64).max(1e-12);
            e += p * p;
            log_sum += p.ln();
            lin_sum += p;
        }
        prev_mag.copy_from_slice(&mag);
        let total: f32 = mag.iter().sum();
        flux_share.push(if total > 1e-9 { f / total } else { 0.0 });
        flux.push(f);
        rms.push(((e / bins as f64).sqrt()) as f32);
        let geo = (log_sum / bins as f64).exp();
        let arith = lin_sum / bins as f64;
        flatness.push(if arith > 0.0 { (geo / arith) as f32 } else { 0.0 });
    }

    let onsets = pick_onsets(&flux, &mono, n, hop, sample_rate, settings);
    let (f0_hz, f0_confidence, voiced) = estimate_f0(&mono, sample_rate, hop, steps, settings);

    // Percussivity: share of frames whose spectrum grows sharply.
    //
    // "Well above the median flux" is not enough on its own: a steady tone has
    // a median near zero, so ordinary numerical ripple clears three times it
    // and a held note reads as percussive. The growth also has to be a real
    // share of the frame, which is what separates an attack from a ripple.
    let flux_median = median(&flux);
    let spiky = flux
        .iter()
        .zip(flux_share.iter())
        .filter(|(v, s)| **v > flux_median * 3.0 && **s > 0.15)
        .count();
    let percussivity = if flux.is_empty() { 0.0 } else { spiky as f32 / flux.len() as f32 };
    let loud: Vec<f32> = flatness
        .iter()
        .zip(rms.iter())
        .filter(|(_, r)| **r > 1e-4)
        .map(|(f, _)| *f)
        .collect();
    let tonality = if loud.is_empty() {
        0.0
    } else {
        1.0 - loud.iter().sum::<f32>() / loud.len() as f32
    };
    let voiced_ratio = if voiced.is_empty() {
        0.0
    } else {
        voiced.iter().filter(|v| **v).count() as f32 / voiced.len() as f32
    };
    let peak = buf.peak();

    let class = classify(peak, percussivity, tonality, voiced_ratio);
    let notes = segment_notes(&f0_hz, &f0_confidence, &voiced, hop, sample_rate);

    Analysis {
        analyzer_version: ANALYZER_VERSION,
        source_id: source_id.to_string(),
        sample_rate,
        frames: frames as u64,
        hop,
        onsets,
        f0_hz,
        f0_confidence,
        voiced,
        onset_strength: flux,
        rms,
        class,
        percussivity,
        tonality,
        notes,
    }
}

/// Below this share of transient-like frames there is nothing for a
/// harmonic/percussive split to separate, so the extra machinery only costs
/// accuracy.
const HPSS_WORTH_IT: f32 = 0.05;

fn classify(peak: f32, percussivity: f32, tonality: f32, voiced_ratio: f32) -> ContentClass {
    if peak < 1e-5 {
        return ContentClass::Silence;
    }
    if percussivity > 0.12 && tonality < 0.75 {
        return ContentClass::Percussive;
    }
    if voiced_ratio > 0.6 && tonality > 0.8 {
        return ContentClass::MonophonicTonal;
    }
    // `Mixed` routes to Hybrid, and Hybrid earns its separation only when there
    // is percussive content to separate. A track that is merely not *very*
    // tonal — a dense arrangement, some noise floor — was falling through to
    // here on tonality alone and paying for a split it had no use for. Judge on
    // percussivity first.
    if percussivity < HPSS_WORTH_IT {
        return ContentClass::PolyphonicTonal;
    }
    if tonality > 0.85 {
        return ContentClass::PolyphonicTonal;
    }
    ContentClass::Mixed
}

fn median(v: &[f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 2]
}

/// Sharpen an onset the STFT could only place to within a window.
///
/// A flux peak at frame `m` means the energy appeared somewhere inside the
/// window that frame covers, which at a 2048-sample window is up to 42 ms of
/// slack at 48 kHz — far too coarse for a slice cut or a promoted anchor. This
/// finds the steepest short-term energy rise in that span instead.
fn refine_onset(mono: &[f32], frame_start: usize, window: usize, hop: usize) -> u64 {
    const W: usize = 32;
    let from = frame_start.saturating_sub(hop);
    let to = (frame_start + window + hop).min(mono.len());
    if to <= from + 2 * W {
        return frame_start as u64;
    }
    let mut best = from + W;
    let mut best_rise = f32::NEG_INFINITY;
    let mut back: f32 = mono[from..from + W].iter().map(|v| v.abs()).sum();
    let mut fwd: f32 = mono[from + W..from + 2 * W].iter().map(|v| v.abs()).sum();
    for i in (from + W)..(to - W) {
        let rise = fwd - back;
        if rise > best_rise {
            best_rise = rise;
            best = i;
        }
        back += mono[i].abs() - mono[i - W].abs();
        fwd += mono[i + W].abs() - mono[i].abs();
    }
    best as u64
}

fn pick_onsets(
    flux: &[f32],
    mono: &[f32],
    window: usize,
    hop: usize,
    sample_rate: u32,
    settings: &AnalysisSettings,
) -> Vec<Onset> {
    if flux.len() < 3 {
        return Vec::new();
    }
    // Adaptive threshold: local median plus a multiple of the local median
    // absolute deviation, so a quiet passage is not flooded with onsets.
    let w = 21usize;
    let min_gap = ((settings.min_onset_spacing * sample_rate as f32) as usize / hop).max(1);
    let mut out: Vec<Onset> = Vec::new();
    let mut scratch = vec![0.0f32; w];
    for i in 1..flux.len() - 1 {
        if flux[i] <= flux[i - 1] || flux[i] < flux[i + 1] {
            continue;
        }
        let lo = i.saturating_sub(w / 2);
        let hi = (i + w / 2 + 1).min(flux.len());
        let len = hi - lo;
        scratch[..len].copy_from_slice(&flux[lo..hi]);
        let s = &mut scratch[..len];
        s.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = s[len / 2];
        let mad = s.iter().map(|v| (v - med).abs()).sum::<f32>() / len as f32;
        let thr = med + settings.onset_threshold * mad.max(1e-6);
        if flux[i] > thr {
            let frame = refine_onset(mono, i * hop, window, hop);
            if let Some(last) = out.last() {
                if frame < last.frame + (min_gap * hop) as u64 {
                    continue;
                }
            }
            out.push(Onset { frame, strength: flux[i] / med.max(1e-6) });
        }
    }
    out
}

/// YIN, cut down to what is needed here: the cumulative mean normalised
/// difference function with parabolic refinement. Confidence is `1 - d'`, and
/// a low value is reported rather than hidden, because the design calls for
/// showing confidence and letting the user correct the result (research.md
/// sec.3).
fn estimate_f0(
    mono: &[f32],
    sample_rate: u32,
    hop: usize,
    steps: usize,
    settings: &AnalysisSettings,
) -> (Vec<f32>, Vec<f32>, Vec<bool>) {
    let tau_min = (sample_rate as f32 / settings.f0_max).floor().max(2.0) as usize;
    let tau_max = (sample_rate as f32 / settings.f0_min).ceil() as usize;
    let win = (tau_max * 2).min(mono.len().max(1));
    let mut f0 = vec![0.0f32; steps];
    let mut conf = vec![0.0f32; steps];
    let mut voiced = vec![false; steps];
    if mono.len() < win + 1 || steps == 0 {
        return (f0, conf, voiced);
    }
    let mut d = vec![0.0f32; tau_max + 1];
    let mut dn = vec![0.0f32; tau_max + 1];

    for m in 0..steps {
        let start = m * hop;
        if start + win >= mono.len() {
            break;
        }
        let x = &mono[start..start + win];
        let half = win / 2;
        let energy: f32 = x[..half].iter().map(|v| v * v).sum();
        if energy < 1e-8 {
            continue;
        }
        for tau in 1..=tau_max.min(half - 1) {
            let mut acc = 0.0f32;
            for i in 0..half {
                let diff = x[i] - x[i + tau];
                acc += diff * diff;
            }
            d[tau] = acc;
        }
        // Cumulative mean normalised difference.
        let mut running = 0.0f32;
        dn[0] = 1.0;
        for tau in 1..=tau_max.min(half - 1) {
            running += d[tau];
            dn[tau] = if running > 0.0 { d[tau] * tau as f32 / running } else { 1.0 };
        }
        // First minimum under the threshold, else the global minimum.
        let mut best = tau_min;
        let mut best_val = f32::INFINITY;
        let mut chosen = None;
        for tau in tau_min..=tau_max.min(half - 2) {
            if dn[tau] < best_val {
                best_val = dn[tau];
                best = tau;
            }
            if dn[tau] < 0.15 && dn[tau] <= dn[tau + 1] {
                chosen = Some(tau);
                break;
            }
        }
        let tau = chosen.unwrap_or(best);
        // Parabolic refinement around the chosen lag.
        let refined = if tau > 1 && tau + 1 <= tau_max {
            let (a, b, c) = (dn[tau - 1], dn[tau], dn[tau + 1]);
            let denom = a - 2.0 * b + c;
            if denom.abs() > 1e-9 {
                tau as f32 + 0.5 * (a - c) / denom
            } else {
                tau as f32
            }
        } else {
            tau as f32
        };
        let hz = sample_rate as f32 / refined.max(1.0);
        let c = (1.0 - dn[tau]).clamp(0.0, 1.0);
        f0[m] = hz;
        conf[m] = c;
        voiced[m] = c > 0.6 && hz >= settings.f0_min && hz <= settings.f0_max;
    }

    // Continuity: an isolated octave jump against stable neighbours is the
    // classic YIN failure, so halve or double it back into line.
    for m in 1..steps.saturating_sub(1) {
        if !voiced[m] || !voiced[m - 1] || !voiced[m + 1] {
            continue;
        }
        let (a, b, c) = (f0[m - 1], f0[m], f0[m + 1]);
        let neighbour = (a + c) * 0.5;
        for factor in [0.5f32, 2.0] {
            if (b * factor - neighbour).abs() < (b - neighbour).abs() * 0.5 {
                f0[m] = b * factor;
                break;
            }
        }
    }
    (f0, conf, voiced)
}

/// Group voiced frames into notes. Boundaries are where voicing stops or the
/// pitch moves more than a semitone and stays there.
fn segment_notes(
    f0: &[f32],
    conf: &[f32],
    voiced: &[bool],
    hop: usize,
    _sample_rate: u32,
) -> Vec<DetectedNote> {
    let mut notes = Vec::new();
    let mut i = 0usize;
    let mut next_id = 1u64;
    while i < voiced.len() {
        if !voiced[i] {
            i += 1;
            continue;
        }
        let start = i;
        let mut ref_hz = f0[i];
        let mut j = i;
        while j < voiced.len() && voiced[j] {
            let ratio = f0[j] / ref_hz.max(1e-6);
            if ratio > 1.06 || ratio < 0.945 {
                // Confirm the move lasts before splitting on it.
                let stable = (j..(j + 3).min(voiced.len())).all(|k| {
                    voiced[k] && (f0[k] / ref_hz.max(1e-6) > 1.06 || f0[k] / ref_hz.max(1e-6) < 0.945)
                });
                if stable {
                    break;
                }
            }
            ref_hz = ref_hz * 0.9 + f0[j] * 0.1;
            j += 1;
        }
        if j > start + 2 {
            let mut hz: Vec<f32> = f0[start..j].to_vec();
            hz.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median_hz = hz[hz.len() / 2] as f64;
            let c = conf[start..j].iter().sum::<f32>() / (j - start) as f32;
            notes.push(DetectedNote {
                id: NoteId(next_id),
                source_start: (start * hop) as u64,
                source_end: (j * hop) as u64,
                median_hz,
                confidence: c as f64,
                voiced_ratio: 1.0,
            });
            next_id += 1;
        }
        i = j.max(start + 1);
    }
    notes
}
