//! Deterministic test signals.
//!
//! The analytical row of the corpus: silence, impulse trains, sines, chirps and
//! stereo fixtures whose correct answer is known in closed form, so a failure
//! points at the engine rather than at a listening opinion (validation.md
//! sec.2). Everything here is generated, so there is no licensing question.

use crate::audio::AudioBuffer;
use std::f64::consts::TAU;

pub fn silence(channels: usize, frames: usize) -> AudioBuffer {
    AudioBuffer::silence(channels, frames)
}

pub fn sine(frames: usize, sample_rate: u32, hz: f64, amp: f32, channels: usize) -> AudioBuffer {
    let w = TAU * hz / sample_rate as f64;
    let plane: Vec<f32> = (0..frames).map(|i| (amp as f64 * (w * i as f64).sin()) as f32).collect();
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}

/// Sine plus harmonics at 1/n amplitude: a stand-in for a tonal instrument.
pub fn harmonics(
    frames: usize,
    sample_rate: u32,
    hz: f64,
    partials: usize,
    channels: usize,
) -> AudioBuffer {
    let plane: Vec<f32> = (0..frames)
        .map(|i| {
            let t = i as f64 / sample_rate as f64;
            let mut v = 0.0;
            for h in 1..=partials {
                v += (TAU * hz * h as f64 * t).sin() / h as f64;
            }
            (v * 0.3) as f32
        })
        .collect();
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}

/// Unit impulses every `period` frames. The onset and anchor gates use this
/// because the correct output position is an exact integer.
pub fn impulse_train(
    frames: usize,
    period: usize,
    channels: usize,
    offset: usize,
) -> AudioBuffer {
    let mut plane = vec![0.0f32; frames];
    let mut i = offset;
    while i < frames {
        plane[i] = 1.0;
        i += period.max(1);
    }
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}

/// Percussive hits: a short noise burst with an exponential decay at each
/// impulse position, so onsets are detectable but the tail is real audio.
pub fn drum_hits(
    frames: usize,
    sample_rate: u32,
    period: usize,
    channels: usize,
) -> AudioBuffer {
    let mut plane = vec![0.0f32; frames];
    let decay = (sample_rate as f64 * 0.12) as usize;
    let mut rng = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        ((rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };
    let mut start = 0usize;
    while start < frames {
        let tone = 90.0 + 40.0 * ((start / period.max(1)) % 3) as f64;
        for i in 0..decay.min(frames - start) {
            let env = (-(i as f64) / (sample_rate as f64 * 0.03)).exp();
            let body = (TAU * tone * i as f64 / sample_rate as f64).sin();
            let noise = next() * (-(i as f64) / (sample_rate as f64 * 0.004)).exp();
            plane[start + i] += ((body * 0.6 + noise * 0.6) * env * 0.7) as f32;
        }
        start += period.max(1);
    }
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}

/// Linear chirp, for alias and resolution checks.
pub fn chirp(
    frames: usize,
    sample_rate: u32,
    from_hz: f64,
    to_hz: f64,
    channels: usize,
) -> AudioBuffer {
    let dur = frames as f64 / sample_rate as f64;
    let k = (to_hz - from_hz) / dur.max(1e-9);
    let plane: Vec<f32> = (0..frames)
        .map(|i| {
            let t = i as f64 / sample_rate as f64;
            (TAU * (from_hz * t + 0.5 * k * t * t)).sin() as f32 * 0.5
        })
        .collect();
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}

/// A source-filter vowel: a buzzy glottal source through three fixed
/// resonances. Formant handling is checked against this because the resonance
/// frequencies are known and must not move when the pitch does.
pub fn vowel(
    frames: usize,
    sample_rate: u32,
    f0: f64,
    formants: [f64; 3],
    channels: usize,
) -> AudioBuffer {
    let mut source = vec![0.0f32; frames];
    let period = (sample_rate as f64 / f0).round().max(2.0) as usize;
    for (i, s) in source.iter_mut().enumerate() {
        // Sawtooth-like glottal pulse: rich enough to excite every formant.
        let phase = (i % period) as f64 / period as f64;
        *s = (2.0 * phase - 1.0) as f32 * 0.3;
    }
    let mut out = vec![0.0f32; frames];
    for (fi, &fc) in formants.iter().enumerate() {
        let q = 12.0;
        let w = TAU * fc / sample_rate as f64;
        let r = (-w / (2.0 * q)).exp();
        let a1 = -2.0 * r * w.cos();
        let a2 = r * r;
        let gain = (1.0 - r) * 0.6 / (fi as f64 + 1.0);
        let (mut y1, mut y2) = (0.0f64, 0.0f64);
        for i in 0..frames {
            let y = gain * source[i] as f64 - a1 * y1 - a2 * y2;
            y2 = y1;
            y1 = y;
            out[i] += y as f32;
        }
    }
    let peak = out.iter().fold(0.0f32, |a, v| a.max(v.abs())).max(1e-9);
    for v in out.iter_mut() {
        *v *= 0.7 / peak;
    }
    AudioBuffer::from_planar(vec![out; channels]).expect("uniform planes")
}

/// A sustained bass note with drum hits over it.
///
/// This is the case a mastered track actually presents and that no single-source
/// fixture catches: the kick makes the spectral flux jump, but the bass note
/// underneath it is *continuous*. Anything that reacts to the transient by
/// restarting phase everywhere breaks the bass exactly where the drums are,
/// which is heard as the low end cracking on some beats and not others.
pub fn bass_under_hits(
    frames: usize,
    sample_rate: u32,
    f0: f64,
    period: usize,
    channels: usize,
) -> AudioBuffer {
    let bass = harmonics(frames, sample_rate, f0, 4, 1);
    let hits = drum_hits(frames, sample_rate, period, 1);
    let plane: Vec<f32> = bass
        .channel(0)
        .iter()
        .zip(hits.channel(0).iter())
        .map(|(b, h)| b * 1.1 + h * 0.45)
        .collect();
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}

/// Exponentially decaying noise: a stand-in for a reverb tail.
///
/// A tail is the hardest thing for a phase vocoder to keep smooth, because it
/// is dense and has no partials to lock onto. Its envelope is a straight line
/// in dB, so any roughness the engine adds is measurable as deviation from that
/// line rather than a matter of opinion.
pub fn noise_tail(frames: usize, sample_rate: u32, rt60_s: f64, channels: usize) -> AudioBuffer {
    let mut rng = 0x51ED_270Bu64 | 1;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        ((rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };
    let tau = rt60_s * sample_rate as f64 / 6.9078;
    let planes: Vec<Vec<f32>> = (0..channels)
        .map(|_| {
            (0..frames)
                .map(|i| {
                    let env = (-(i as f64) / tau).exp();
                    (next() * env * 0.5) as f32
                })
                .collect()
        })
        .collect();
    AudioBuffer::from_planar(planes).expect("uniform planes")
}

/// Stereo fixture with one channel inverted. A correct engine keeps the
/// inversion; one that decides per channel does not.
pub fn inverted_stereo(mono: &AudioBuffer) -> AudioBuffer {
    let a = mono.channel(0).to_vec();
    let b: Vec<f32> = a.iter().map(|v| -v).collect();
    AudioBuffer::from_planar(vec![a, b]).expect("two planes")
}

/// Stereo fixture with a fixed inter-channel delay, standing in for two
/// microphones on one source.
pub fn delayed_stereo(mono: &AudioBuffer, delay: usize) -> AudioBuffer {
    let a = mono.channel(0).to_vec();
    let mut b = vec![0.0f32; a.len()];
    for i in delay..a.len() {
        b[i] = a[i - delay];
    }
    AudioBuffer::from_planar(vec![a, b]).expect("two planes")
}

/// Two tones an octave apart plus a hit track: the "mixed" case for Auto.
pub fn mixed(frames: usize, sample_rate: u32, channels: usize) -> AudioBuffer {
    let tone = harmonics(frames, sample_rate, 220.0, 6, 1);
    let hits = drum_hits(frames, sample_rate, sample_rate as usize / 2, 1);
    let plane: Vec<f32> = tone
        .channel(0)
        .iter()
        .zip(hits.channel(0).iter())
        .map(|(a, b)| a * 0.6 + b * 0.6)
        .collect();
    AudioBuffer::from_planar(vec![plane; channels]).expect("uniform planes")
}
