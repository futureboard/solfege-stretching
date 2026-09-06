//! Band-limited fractional resampling.
//!
//! Reading a source faster than 1x folds everything above the new Nyquist back
//! into the band, so the kernel cutoff has to track the read rate (dsp.md
//! sec.3). Cutoff varies continuously under a piecewise-linear map, so the
//! bank holds a small set of kernels built at prepare time and picks the
//! nearest; the spacing is fine enough that the residual cutoff error is far
//! below the aliasing it is there to suppress. No allocation happens after
//! `new`.

use super::window::blackman_harris;

/// Kernel half-width in zero crossings. 16 gives roughly -90 dB stopband with
/// the Blackman-Harris window used here.
pub const DEFAULT_HALF_TAPS: usize = 16;
/// Sub-sample table resolution.
pub const DEFAULT_OVERSAMPLE: usize = 256;

pub struct SincBank {
    half: usize,
    over: usize,
    cutoffs: Vec<f32>,
    /// One table per cutoff, `half * over + 2` entries, symmetric around zero.
    tables: Vec<Vec<f32>>,
}

impl SincBank {
    /// `max_speed` is the fastest source read rate the plan can ask for.
    pub fn new(half: usize, over: usize, max_speed: f64) -> Self {
        let max_speed = max_speed.max(1.0);
        // Cutoffs from 1/max_speed to 1.0. One table is enough when nothing
        // ever reads faster than 1x.
        let lo = (1.0 / max_speed).clamp(0.05, 1.0);
        let n = if (1.0 - lo) < 1e-6 {
            1
        } else {
            (((1.0 - lo) / 0.02).ceil() as usize + 1).clamp(2, 64)
        };
        let mut cutoffs = Vec::with_capacity(n);
        let mut tables = Vec::with_capacity(n);
        for i in 0..n {
            let c = if n == 1 { 1.0 } else { lo + (1.0 - lo) * i as f64 / (n - 1) as f64 };
            cutoffs.push(c as f32);
            tables.push(build_table(half, over, c));
        }
        Self { half, over, cutoffs, tables }
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    #[inline]
    fn table_for(&self, cutoff: f32) -> &[f32] {
        if self.cutoffs.len() == 1 {
            return &self.tables[0];
        }
        let lo = self.cutoffs[0];
        let hi = self.cutoffs[self.cutoffs.len() - 1];
        let x = ((cutoff - lo) / (hi - lo)).clamp(0.0, 1.0);
        let idx = (x * (self.cutoffs.len() - 1) as f32).round() as usize;
        &self.tables[idx.min(self.tables.len() - 1)]
    }

    #[inline]
    fn kernel(table: &[f32], over: usize, x: f32) -> f32 {
        // `x` is a distance in samples; the table is sampled every 1/over.
        let pos = x.abs() * over as f32;
        let i = pos as usize;
        if i + 1 >= table.len() {
            return 0.0;
        }
        let f = pos - i as f32;
        table[i] * (1.0 - f) + table[i + 1] * f
    }

    /// Interpolate `x` at fractional position `pos`, band-limited to `cutoff`
    /// (1.0 = full band). Positions outside the buffer read as zero, so the
    /// caller decides padding by what it puts in the buffer.
    pub fn read(&self, x: &[f32], pos: f64, cutoff: f32) -> f32 {
        let table = self.table_for(cutoff);
        let base = pos.floor();
        let frac = (pos - base) as f32;
        let base = base as i64;
        let half = self.half as i64;
        let mut acc = 0.0f32;
        let mut i = -half + 1;
        while i <= half {
            let idx = base + i;
            if idx >= 0 && (idx as usize) < x.len() {
                let d = i as f32 - frac;
                acc += x[idx as usize] * Self::kernel(table, self.over, d);
            }
            i += 1;
        }
        acc
    }

    /// Frames of context the kernel needs on each side of a read position.
    pub fn context(&self) -> usize {
        self.half + 1
    }
}

fn build_table(half: usize, over: usize, cutoff: f64) -> Vec<f32> {
    let n = half * over + 2;
    let win = blackman_harris(2 * half * over + 1);
    let mut t = Vec::with_capacity(n);
    for i in 0..n {
        let x = i as f64 / over as f64;
        let s = if x == 0.0 {
            1.0
        } else {
            let a = std::f64::consts::PI * cutoff * x;
            a.sin() / a
        };
        let wi = half * over + i;
        let w = if wi < win.len() { win[wi] as f64 } else { 0.0 };
        t.push((cutoff * s * w) as f32);
    }
    t
}

/// Read a whole planar buffer at a constant speed. Used by Tape and by the
/// pitch stage when the ratio does not vary.
pub fn resample_constant(
    input: &[Vec<f32>],
    speed: f64,
    out_frames: usize,
    bank: &SincBank,
) -> Vec<Vec<f32>> {
    let cutoff = (1.0 / speed).min(1.0) as f32;
    input
        .iter()
        .map(|plane| {
            (0..out_frames)
                .map(|i| bank.read(plane, i as f64 * speed, cutoff))
                .collect()
        })
        .collect()
}
