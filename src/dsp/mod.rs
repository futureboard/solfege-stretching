//! Shared DSP primitives: windows, resampling kernels, correlation and STFT.

pub mod corr;
pub mod resample;
pub mod stft;
pub mod window;

/// Semitones to a frequency multiplier.
#[inline]
pub fn semitones_to_ratio(st: f64) -> f64 {
    2f64.powf(st / 12.0)
}

/// Frequency multiplier to semitones.
#[inline]
pub fn ratio_to_semitones(r: f64) -> f64 {
    12.0 * r.log2()
}

/// Cents between two frequencies.
#[inline]
pub fn cents(a: f64, b: f64) -> f64 {
    1200.0 * (a / b).log2()
}

#[inline]
pub fn db_to_gain(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

#[inline]
pub fn gain_to_db(g: f32) -> f64 {
    20.0 * (g.abs().max(1e-12) as f64).log10()
}
