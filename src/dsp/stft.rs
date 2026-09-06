//! Real FFT front end for the phase vocoder.
//!
//! `realfft` is used rather than a complex FFT because it enforces the
//! real-signal constraints at DC and Nyquist by construction, which dsp.md
//! sec.5 calls out as an easy place to get wrong. All scratch is allocated in
//! `new`; `forward`/`inverse` touch no allocator.

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

pub struct RealStft {
    size: usize,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    time_scratch: Vec<f32>,
    fwd_scratch: Vec<Complex32>,
    inv_scratch: Vec<Complex32>,
    spec_scratch: Vec<Complex32>,
}

impl RealStft {
    pub fn new(size: usize) -> Self {
        assert!(size >= 8 && size.is_power_of_two(), "STFT size must be a power of two >= 8");
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(size);
        let inv = planner.plan_fft_inverse(size);
        let fwd_scratch = fwd.make_scratch_vec();
        let inv_scratch = inv.make_scratch_vec();
        Self {
            size,
            time_scratch: vec![0.0; size],
            spec_scratch: vec![Complex32::new(0.0, 0.0); size / 2 + 1],
            fwd,
            inv,
            fwd_scratch,
            inv_scratch,
        }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }
    #[inline]
    pub fn bins(&self) -> usize {
        self.size / 2 + 1
    }

    /// `time` must be `size` long; `spec` must be `bins()` long.
    pub fn forward(&mut self, time: &[f32], spec: &mut [Complex32]) {
        self.time_scratch.copy_from_slice(time);
        self.fwd
            .process_with_scratch(&mut self.time_scratch, spec, &mut self.fwd_scratch)
            .expect("stft sizes checked at construction");
    }

    /// Inverse transform with the 1/N normalisation applied, so that
    /// `inverse(forward(x)) == x`.
    pub fn inverse(&mut self, spec: &[Complex32], time: &mut [f32]) {
        self.spec_scratch.copy_from_slice(spec);
        // realfft requires imaginary parts of DC and Nyquist to be zero.
        let last = self.spec_scratch.len() - 1;
        self.spec_scratch[0].im = 0.0;
        self.spec_scratch[last].im = 0.0;
        self.inv
            .process_with_scratch(&mut self.spec_scratch, time, &mut self.inv_scratch)
            .expect("stft sizes checked at construction");
        let scale = 1.0 / self.size as f32;
        for v in time.iter_mut() {
            *v *= scale;
        }
    }
}

/// Wrap a phase difference into `-pi..pi`.
#[inline]
pub fn principal_arg(x: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut a = x;
    a -= TAU * ((a + PI) / TAU).floor();
    a
}

/// Magnitude spectrum, reused by analysis and by the hybrid separator.
pub fn magnitudes(spec: &[Complex32], out: &mut [f32]) {
    for (o, c) in out.iter_mut().zip(spec.iter()) {
        *o = c.norm();
    }
}
