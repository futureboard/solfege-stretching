//! Analysis and synthesis windows.

/// Periodic Hann, the right form for STFT overlap-add (the symmetric form is
/// for filter design and does not sum flat).
pub fn hann_periodic(n: usize) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    (0..n)
        .map(|i| {
            let x = std::f64::consts::TAU * i as f64 / n as f64;
            (0.5 - 0.5 * x.cos()) as f32
        })
        .collect()
}

/// sqrt(Hann): applied on both analysis and synthesis it gives a Hann product,
/// which is what the OLA normalisation in dsp.md sec.4 assumes.
pub fn sqrt_hann_periodic(n: usize) -> Vec<f32> {
    hann_periodic(n).into_iter().map(|v| v.max(0.0).sqrt()).collect()
}

/// Blackman-Harris, used to window the resampler kernel.
pub fn blackman_harris(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = std::f64::consts::TAU * i as f64 / (n - 1).max(1) as f64;
            (0.35875 - 0.48829 * x.cos() + 0.14128 * (2.0 * x).cos()
                - 0.01168 * (3.0 * x).cos()) as f32
        })
        .collect()
}

/// Equal-power crossfade pair over `n` frames.
pub fn equal_power_fade(n: usize) -> (Vec<f32>, Vec<f32>) {
    let mut out_a = Vec::with_capacity(n);
    let mut out_b = Vec::with_capacity(n);
    for i in 0..n {
        let x = (i as f64 + 0.5) / n as f64;
        let a = (std::f64::consts::FRAC_PI_2 * x).cos();
        let b = (std::f64::consts::FRAC_PI_2 * x).sin();
        out_a.push(a as f32);
        out_b.push(b as f32);
    }
    (out_a, out_b)
}

/// Linear (equal-gain) crossfade pair. Correct choice when the two signals are
/// correlated, where equal power would boost the sum (system-design.md sec.11).
pub fn linear_fade(n: usize) -> (Vec<f32>, Vec<f32>) {
    let mut out_a = Vec::with_capacity(n);
    let mut out_b = Vec::with_capacity(n);
    for i in 0..n {
        let x = ((i as f64 + 0.5) / n as f64) as f32;
        out_a.push(1.0 - x);
        out_b.push(x);
    }
    (out_a, out_b)
}
