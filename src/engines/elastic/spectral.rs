//! The spectral kernel behind every Elastic mode: one analysis frame in, one
//! synthesis frame out, at whatever analysis hop the scheduler chose.
//!
//! 1. **Analysis.** One real FFT of the raw frame per channel. The periodic
//!    Hann window and its derivative both have three-tap spectra, so the
//!    windowed spectrum and the derivative-window spectrum come out of the raw
//!    one by convolution. Frames use zero-phase framing (centre at index 0).
//!    The derivative window gives every bin's reassigned frequency in closed
//!    form, `omega = omega_k - Im(X_dh conj X_h) / |X_h|^2`, which is what the
//!    phase advance is unwrapped around - so a long analysis hop (a strong
//!    compression) cannot unwrap to the wrong partial.
//! 2. **Phase.** Phase-gradient heap integration (Průša & Holighaus, "Phase
//!    Vocoder Done Right", EUSIPCO 2017), written in rotation form: the output
//!    bin is the input bin times `e^{j theta}`, and `theta` is the only
//!    unknown. Bins are settled loudest first; each takes its rotation either
//!    from its own previous frame (horizontal: the measured phase advance,
//!    rescaled from the analysis hop to the synthesis hop) or from an already
//!    settled neighbour in the same frame (vertical: copy the rotation, which
//!    keeps the input's phase relation between neighbouring bins - its local
//!    group delay - and therefore the shape of anything broadband). At ratio
//!    1 the rotation stays exactly zero and the kernel is an identity.
//! 3. **Attacks.** In frames the scheduler marks as containing an onset, bins
//!    that jumped by more than 6 dB are re-seeded from the analysed phase
//!    instead of inheriting a rotation from whatever rang before. Inheriting
//!    one would rotate every bin of the click by the same arbitrary angle - a
//!    Hilbert-like distortion that softens the attack and leaves a slow skirt
//!    either side of it.
//! 4. **Channels.** One rotation per bin, applied to every channel. The
//!    horizontal step measures the common rotation across channels (an
//!    energy-weighted cross-power sum, polarity-safe), so an inverted or
//!    delayed pair keeps exactly the relation it came in with.
//! 5. **Formants.** When the envelope must not follow a transposition, a
//!    cepstrally smoothed log spectrum gives the correction
//!    `env(k * shift) / env(k)`, applied before the resampler moves it.
//!
//! Every buffer is allocated in `new`; `frame` never touches the allocator.

use crate::dsp::stft::{principal_arg, RealStft};
use realfft::num_complex::Complex32;
use std::collections::BinaryHeap;
use std::f32::consts::{PI, TAU};

/// Bins below this fraction of the frame's loudest bin take the horizontal
/// estimate without a heap entry. -80 dB.
const TOLERANCE: f32 = 1e-4;
/// Formant correction is clamped to this range (log energy units).
const FORMANT_MAX_BOOST: f32 = 18.0 * std::f32::consts::LN_10 / 10.0;
const FORMANT_MAX_CUT: f32 = 30.0 * std::f32::consts::LN_10 / 10.0;

/// Everything that may change from one frame to the next.
#[derive(Copy, Clone, Debug)]
pub struct FrameParams {
    /// Analysis hop that led to this frame, in source frames. `None` for the
    /// first frame after a start or seek.
    pub analysis_hop: Option<i64>,
    /// Envelope shift applied here: output envelope at bin `k` is the input
    /// envelope at `k * envelope_shift`. 1 leaves it alone.
    pub envelope_shift: f32,
    /// An onset lies inside this frame's window.
    pub transient: bool,
}

pub struct SpectralKernel {
    n: usize,
    bins: usize,
    hop: usize,
    channels: usize,
    fft: RealStft,
    window: Vec<f32>,
    /// Constant OLA divisor: `sum(w^2) / hop`.
    norm: f32,

    time: Vec<f32>,
    scratch: Vec<Complex32>,
    x: Vec<Vec<Complex32>>,
    x_prev: Vec<Vec<Complex32>>,
    y_prev: Vec<Vec<Complex32>>,
    omega: Vec<f32>,
    energy: Vec<f32>,
    log_env: Vec<f32>,
    gain: Vec<f32>,
    mag: Vec<f32>,
    mag_prev: Vec<f32>,
    advance: Vec<f32>,
    theta: Vec<f32>,
    done: Vec<bool>,
    heap: BinaryHeap<u64>,
    ceps: Vec<Complex32>,
    have_prev: bool,
    /// Cepstral lifter cut-off, in samples of quefrency.
    lifter: usize,
}

impl SpectralKernel {
    pub fn new(n: usize, hop: usize, channels: usize, sample_rate: u32) -> Self {
        assert!(n.is_power_of_two() && n >= 64, "window must be a power of two");
        assert!(hop > 0 && hop <= n / 4, "the OLA normalisation needs at least 4x overlap");
        let bins = n / 2 + 1;
        let window: Vec<f32> = (0..n)
            .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()) as f32)
            .collect();
        let wsum: f64 = window.iter().map(|w| (*w as f64) * (*w as f64)).sum();
        let zero = Complex32::new(0.0, 0.0);
        // 1.5 ms of quefrency: smooths over the harmonics of voices up to
        // ~650 Hz while keeping the formant shape.
        let lifter = ((sample_rate as f64 * 0.0015).round() as usize).clamp(8, n / 4);
        Self {
            n,
            bins,
            hop,
            channels,
            fft: RealStft::new(n),
            window,
            norm: (wsum / hop as f64) as f32,
            time: vec![0.0; n],
            scratch: vec![zero; bins],
            x: vec![vec![zero; bins]; channels],
            x_prev: vec![vec![zero; bins]; channels],
            y_prev: vec![vec![zero; bins]; channels],
            omega: vec![0.0; bins],
            energy: vec![0.0; bins],
            log_env: vec![0.0; bins],
            gain: vec![1.0; bins],
            mag: vec![0.0; bins],
            mag_prev: vec![0.0; bins],
            advance: vec![0.0; bins],
            theta: vec![0.0; bins],
            done: vec![false; bins],
            heap: BinaryHeap::with_capacity(2 * bins + 8),
            ceps: vec![zero; bins],
            have_prev: false,
            lifter,
        }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.n
    }
    #[inline]
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Forget all phase history: the next frame is reconstructed as analysed.
    pub fn reset(&mut self) {
        self.have_prev = false;
        for c in 0..self.channels {
            self.x_prev[c].fill(Complex32::new(0.0, 0.0));
            self.y_prev[c].fill(Complex32::new(0.0, 0.0));
        }
        self.mag_prev.fill(0.0);
        self.theta.fill(0.0);
    }

    /// Process one frame.
    ///
    /// `read(c, i)` returns sample `i` (0..n) of the analysis frame for
    /// channel `c`; `i = n/2` is the frame centre. `write(c, i, v)` receives
    /// the windowed synthesis frame, already divided by the OLA constant, for
    /// the caller to overlap-add.
    pub fn frame<R, W>(&mut self, params: FrameParams, mut read: R, mut write: W)
    where
        R: FnMut(usize, usize) -> f32,
        W: FnMut(usize, usize, f32),
    {
        let n = self.n;
        let half = n / 2;
        let bins = self.bins;
        let ch = self.channels;

        // ---- 1. analysis -------------------------------------------------
        self.energy.fill(0.0);
        self.omega.fill(0.0);
        let scale_d = PI / n as f32 * 0.5;
        for c in 0..ch {
            std::mem::swap(&mut self.x[c], &mut self.x_prev[c]);
            for i in 0..n {
                self.time[i] = read(c, i);
            }
            self.fft.forward(&self.time, &mut self.scratch);
            let raw = &self.scratch;
            let at = |k: isize| -> Complex32 {
                if k < 0 {
                    raw[(-k) as usize].conj()
                } else if k as usize >= bins {
                    raw[2 * (bins - 1) - k as usize].conj()
                } else {
                    raw[k as usize]
                }
            };
            let xc = &mut self.x[c];
            for b in 0..bins {
                let k = b as isize;
                let lo = at(k - 1);
                let hi = at(k + 1);
                //   X_h(k)  =  0.5 R(k) - 0.25 (R(k-1) + R(k+1))
                //   X_dh(k) = (pi/n) (R(k-1) - R(k+1)) / (2j)
                let mut h = raw[b] * 0.5 - (lo + hi) * 0.25;
                let dd = lo - hi;
                let mut d = Complex32::new(dd.im, -dd.re) * scale_d;
                if b & 1 == 1 {
                    h = -h;
                    d = -d;
                }
                xc[b] = h;
                self.omega[b] += d.im * h.re - d.re * h.im; // Im(d conj h)
                self.energy[b] += h.norm_sqr();
            }
        }
        let bin_w = TAU / n as f32;
        for b in 0..bins {
            let e = self.energy[b];
            let dev = if e > 1e-30 { self.omega[b] / e } else { 0.0 };
            // Reassignment cannot move energy further than the mainlobe;
            // clamp so a noise bin cannot claim a wild frequency.
            self.omega[b] = b as f32 * bin_w - dev.clamp(-2.5 * bin_w, 2.5 * bin_w);
        }

        // ---- formant correction -------------------------------------------
        let formant_active = (params.envelope_shift - 1.0).abs() > 1e-6;
        if formant_active {
            self.envelope();
            let f = params.envelope_shift.max(1e-3);
            for k in 0..bins {
                let target = interp(&self.log_env, k as f32 * f);
                let d = (target - self.log_env[k]).clamp(-FORMANT_MAX_CUT, FORMANT_MAX_BOOST);
                self.gain[k] = (0.5 * d).exp();
            }
        }

        // ---- 2. horizontal advance ----------------------------------------
        let hs = self.hop as f32;
        let ha = params.analysis_hop.filter(|_| self.have_prev);
        let mut mmax = 0.0f32;
        for k in 0..bins {
            let m = self.energy[k].sqrt();
            self.mag[k] = m;
            if m > mmax {
                mmax = m;
            }
            let w = self.omega[k];
            self.advance[k] = match ha {
                Some(h) if h > 0 && (h as usize) <= n / 4 => {
                    // Measured phase advance, unwrapped around the
                    // reassigned estimate: this is what makes ratio 1 an
                    // exact identity.
                    let hf = h as f32;
                    let mut cp = Complex32::new(0.0, 0.0);
                    for c in 0..ch {
                        cp += self.x[c][k] * self.x_prev[c][k].conj();
                    }
                    let meas = if cp.norm_sqr() > 0.0 { cp.im.atan2(cp.re) } else { hf * w };
                    (hf * w + principal_arg(meas - hf * w)) * (hs / hf)
                }
                _ => w * hs,
            };
        }

        // ---- 3. rotation ----------------------------------------------------
        if !self.have_prev {
            self.theta.fill(0.0);
        } else {
            self.integrate(mmax, params.transient);
        }

        // ---- 4. synthesis ---------------------------------------------------
        let inv_norm = 1.0 / self.norm;
        for c in 0..ch {
            {
                let yp = &mut self.y_prev[c];
                let xc = &self.x[c];
                for k in 0..bins {
                    let (s, co) = self.theta[k].sin_cos();
                    yp[k] = xc[k] * Complex32::new(co, s);
                }
            }
            if formant_active {
                for k in 0..bins {
                    self.scratch[k] = self.y_prev[c][k] * self.gain[k];
                }
            } else {
                self.scratch.copy_from_slice(&self.y_prev[c]);
            }
            self.fft.inverse(&self.scratch, &mut self.time);
            for i in 0..n {
                let v = self.time[(i + half) & (n - 1)] * self.window[i] * inv_norm;
                write(c, i, v);
            }
        }
        self.mag_prev.copy_from_slice(&self.mag);
        self.have_prev = true;
    }

    /// Cepstrally smoothed log energy of the input, into `log_env`.
    fn envelope(&mut self) {
        let n = self.n;
        let bins = self.bins;
        for b in 0..bins {
            self.ceps[b] = Complex32::new((self.energy[b] + 1e-12).ln(), 0.0);
        }
        self.fft.inverse(&self.ceps, &mut self.time);
        // Keep the low quefrencies (both ends: the cepstrum is even), with a
        // short taper so the lifter itself does not ring.
        let q = self.lifter;
        let taper = (q / 4).max(1);
        for i in 0..n {
            let d = i.min(n - i);
            let g = if d < q - taper {
                1.0
            } else if d < q {
                0.5 + 0.5 * (PI * (d - (q - taper)) as f32 / taper as f32).cos()
            } else {
                0.0
            };
            self.time[i] *= g;
        }
        self.fft.forward(&self.time, &mut self.ceps);
        for b in 0..bins {
            self.log_env[b] = self.ceps[b].re;
        }
    }

    /// RTPGHI over this frame's bins, producing `theta`.
    fn integrate(&mut self, mmax: f32, transient: bool) {
        let bins = self.bins;
        let ch = self.channels;
        let tol = mmax * TOLERANCE;
        // Horizontal candidate for every bin: the common rotation that lines
        // this frame's input up with last frame's output, plus the advance.
        for k in 0..bins {
            let mut cp = Complex32::new(0.0, 0.0);
            for c in 0..ch {
                cp += self.y_prev[c][k] * self.x[c][k].conj();
            }
            let base = if cp.norm_sqr() > 0.0 { cp.im.atan2(cp.re) } else { 0.0 };
            self.theta[k] = principal_arg(base + self.advance[k]);
        }
        let mut remaining = 0usize;
        for k in 0..bins {
            // Too quiet to matter: keep the horizontal estimate.
            self.done[k] = self.mag[k] <= tol;
            if !self.done[k] {
                remaining += 1;
            }
        }
        self.heap.clear();
        if transient {
            for k in 0..bins {
                if !self.done[k] && self.mag[k] > 2.0 * self.mag_prev[k] {
                    self.theta[k] = 0.0;
                    self.done[k] = true;
                    remaining -= 1;
                    self.heap.push(key(self.mag[k], true, k));
                }
            }
        }
        if remaining == 0 {
            return;
        }
        let ptol = self.mag_prev.iter().cloned().fold(0.0f32, f32::max) * TOLERANCE;
        for k in 0..bins {
            if self.mag_prev[k] > ptol && !self.done[k] {
                self.heap.push(key(self.mag_prev[k], false, k));
            }
        }
        while remaining > 0 {
            let Some(top) = self.heap.pop() else {
                // Nothing left to propagate from: seed with the loudest
                // unsettled bin, as analysed.
                let mut best = usize::MAX;
                let mut bm = -1.0f32;
                for k in 0..bins {
                    if !self.done[k] && self.mag[k] > bm {
                        bm = self.mag[k];
                        best = k;
                    }
                }
                if best == usize::MAX {
                    break;
                }
                self.theta[best] = 0.0;
                self.done[best] = true;
                remaining -= 1;
                self.heap.push(key(self.mag[best], true, best));
                continue;
            };
            let (current, k) = unkey(top);
            if !current {
                if !self.done[k] {
                    // horizontal: theta[k] already holds it
                    self.done[k] = true;
                    remaining -= 1;
                    self.heap.push(key(self.mag[k], true, k));
                }
            } else {
                let t = self.theta[k];
                if k > 0 && !self.done[k - 1] {
                    self.theta[k - 1] = t;
                    self.done[k - 1] = true;
                    remaining -= 1;
                    self.heap.push(key(self.mag[k - 1], true, k - 1));
                }
                if k + 1 < bins && !self.done[k + 1] {
                    self.theta[k + 1] = t;
                    self.done[k + 1] = true;
                    remaining -= 1;
                    self.heap.push(key(self.mag[k + 1], true, k + 1));
                }
            }
        }
    }
}

#[inline]
fn key(mag: f32, current: bool, k: usize) -> u64 {
    ((mag.max(0.0).to_bits() as u64) << 32) | ((current as u64) << 31) | k as u64
}

#[inline]
fn unkey(v: u64) -> (bool, usize) {
    (((v >> 31) & 1) == 1, (v & 0x7fff_ffff) as usize)
}

/// Linear interpolation of a per-bin array, clamped at the ends.
#[inline]
fn interp(a: &[f32], x: f32) -> f32 {
    let last = a.len() - 1;
    if x <= 0.0 {
        return a[0];
    }
    if x >= last as f32 {
        return a[last];
    }
    let i = x as usize;
    let f = x - i as f32;
    a[i] + (a[i + 1] - a[i]) * f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(n: usize, hop: usize, signal: &[f32], shift: f32) -> Vec<f32> {
        let mut k = SpectralKernel::new(n, hop, 1, 48_000);
        let len = signal.len();
        let mut out = vec![0.0f32; len + 2 * n];
        let mut centre: i64 = -(n as i64) / 2 + hop as i64;
        let mut first = true;
        while centre < len as i64 + n as i64 / 2 {
            let start = centre - n as i64 / 2;
            k.frame(
                FrameParams {
                    analysis_hop: if first { None } else { Some(hop as i64) },
                    envelope_shift: shift,
                    transient: false,
                },
                |_, i| {
                    let s = start + i as i64;
                    if s >= 0 && (s as usize) < len { signal[s as usize] } else { 0.0 }
                },
                |_, i, v| {
                    let o = start + i as i64 + n as i64;
                    if o >= 0 && (o as usize) < out.len() {
                        out[o as usize] += v;
                    }
                },
            );
            first = false;
            centre += hop as i64;
        }
        out[n..n + len].to_vec()
    }

    #[test]
    fn unity_ratio_reconstructs_the_input() {
        let len = 20_000;
        let sig: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                0.4 * (TAU * 220.0 * t).sin()
                    + 0.2 * (TAU * 1375.3 * t).sin()
                    + if i % 5000 == 100 { 0.8 } else { 0.0 }
            })
            .collect();
        let out = run(2048, 256, &sig, 1.0);
        let mut err = 0.0f64;
        let mut e = 0.0f64;
        for i in 0..len {
            err += ((out[i] - sig[i]) as f64).powi(2);
            e += (sig[i] as f64).powi(2);
        }
        let db = 10.0 * (err / e).log10();
        assert!(db < -80.0, "identity error {db:.1} dB");
    }

    #[test]
    fn reassignment_finds_an_off_bin_frequency() {
        let n = 2048;
        let mut k = SpectralKernel::new(n, 256, 1, 48_000);
        let f0 = 1000.37f32;
        k.frame(
            FrameParams { analysis_hop: None, envelope_shift: 1.0, transient: false },
            |_, i| (TAU * f0 * i as f32 / 48_000.0).sin(),
            |_, _, _| {},
        );
        let bin = (f0 / 48_000.0 * n as f32).round() as usize;
        let hz = k.omega[bin] / TAU * 48_000.0;
        assert!((hz - f0).abs() < 0.5, "reassigned {hz} Hz for {f0} Hz");
    }
}
