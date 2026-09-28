//! Soloist: pitch-synchronous overlap-add for one voice or one instrument.
//!
//! A monophonic line is periodic almost everywhere, and a periodic signal can
//! be stretched and transposed without a phase vocoder at all: cut it into
//! two-period grains centred one period apart, and lay the grains back down at
//! a new spacing. Stretching repeats or skips whole periods; transposing
//! changes the spacing. Each grain is an untouched piece of the original, so
//! there is no phasiness, and the spectral envelope - the formants - stays
//! where it was unless asked to move.
//!
//! The parts:
//!
//! * **Pitch.** YIN (de Cheveigné & Kawahara 2002) on the channel sum, with
//!   the difference function computed through one FFT autocorrelation, on a
//!   fixed grid of source positions so the result does not depend on how the
//!   caller blocks the input. Hysteresis on the voicing decision.
//! * **Marks.** Output marks sit one *output* period apart (`T / p`). The
//!   grain for each mark continues from the previous grain by a whole number
//!   of source periods - the one that keeps the read position closest to
//!   where the map wants it - and a short waveform-similarity search then
//!   pins the exact lag, so neighbouring grains always overlap in phase.
//! * **Unvoiced material** (breath, sibilants, noise) is read straight along
//!   the map with short fixed grains; there is no period to preserve.
//! * **Transients** use the same onset lock as the Elastic modes: a plosive
//!   is read at unity rate and lands where the map puts it.
//! * **Formants** are free: grains are read at rate `f` (1 keeps the
//!   envelope, `p` makes it follow the pitch like tape).
//!
//! Every decision is shared by all channels: one set of marks, one grain
//! position, one lag.

use super::core::{EngineCore, Stepper};
use super::elastic::schedule::{OnsetDetector, Scheduler};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::document::FormantPolicy;
use crate::mapping::TimeMap;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::f64::consts::PI;
use std::sync::Arc;

const MIN_HZ: f64 = 50.0;
const MAX_HZ: f64 = 1000.0;
/// Unvoiced grain half-length, seconds.
const UNVOICED_HALF: f64 = 0.006;
const GLIDE_SECONDS: f64 = 0.035;

#[derive(Copy, Clone, Debug, PartialEq)]
struct PitchEst {
    grid: i64,
    /// Period in source frames; 0 when unvoiced.
    period: f64,
}

/// FFT-based YIN over one window.
struct Yin {
    w: usize,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    buf: Vec<f32>,
    spec: Vec<Complex32>,
    sf: Vec<Complex32>,
    si: Vec<Complex32>,
    corr: Vec<f32>,
    prefix: Vec<f64>,
    cmndf: Vec<f32>,
    tau_min: usize,
    tau_max: usize,
}

impl Yin {
    fn new(sample_rate: u32) -> Self {
        let tau_max = (sample_rate as f64 / MIN_HZ).ceil() as usize;
        let w = (2 * tau_max).next_power_of_two();
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(2 * w);
        let inv = planner.plan_fft_inverse(2 * w);
        let sf = fwd.make_scratch_vec();
        let si = inv.make_scratch_vec();
        Self {
            w,
            buf: vec![0.0; 2 * w],
            spec: vec![Complex32::new(0.0, 0.0); w + 1],
            sf,
            si,
            corr: vec![0.0; 2 * w],
            prefix: vec![0.0; w + 1],
            cmndf: vec![1.0; tau_max + 2],
            tau_min: ((sample_rate as f64 / MAX_HZ).floor() as usize).max(2),
            tau_max: tau_max.min(w / 2),
            fwd,
            inv,
        }
    }

    /// `x` must hold `w` samples. Returns (period, aperiodicity).
    fn estimate(&mut self, x: &[f32]) -> Option<(f64, f32)> {
        let w = self.w;
        let mut energy = 0.0f64;
        self.prefix[0] = 0.0;
        for (i, v) in x.iter().enumerate().take(w) {
            let e = (*v as f64) * (*v as f64);
            energy += e;
            self.prefix[i + 1] = self.prefix[i] + e;
        }
        if energy / (w as f64) < 1e-7 {
            return None; // below -70 dBFS: silence
        }
        self.buf[..w].copy_from_slice(&x[..w]);
        self.buf[w..].fill(0.0);
        self.fwd
            .process_with_scratch(&mut self.buf, &mut self.spec, &mut self.sf)
            .ok()?;
        for c in self.spec.iter_mut() {
            *c = Complex32::new(c.norm_sqr(), 0.0);
        }
        self.inv
            .process_with_scratch(&mut self.spec, &mut self.corr, &mut self.si)
            .ok()?;
        let scale = 1.0 / (2 * w) as f32;
        // d(tau) = sum_{j<w-tau} (x_j - x_{j+tau})^2
        let mut run = 0.0f64;
        self.cmndf[0] = 1.0;
        for tau in 1..=self.tau_max {
            let e0 = self.prefix[w - tau];
            let e1 = self.prefix[w] - self.prefix[tau];
            let r = self.corr[tau] as f64 * scale as f64;
            let d = (e0 + e1 - 2.0 * r).max(0.0);
            run += d;
            self.cmndf[tau] = if run > 0.0 { (d * tau as f64 / run) as f32 } else { 1.0 };
        }
        let lo = self.tau_min;
        let hi = self.tau_max - 1;
        let mut pick = None;
        let mut tau = lo;
        while tau < hi {
            if self.cmndf[tau] < 0.15 {
                while tau + 1 < hi && self.cmndf[tau + 1] < self.cmndf[tau] {
                    tau += 1;
                }
                pick = Some(tau);
                break;
            }
            tau += 1;
        }
        let tau = match pick {
            Some(t) => t,
            None => {
                let mut best = lo;
                for t in lo..hi {
                    if self.cmndf[t] < self.cmndf[best] {
                        best = t;
                    }
                }
                best
            }
        };
        let a = self.cmndf[tau - 1] as f64;
        let b = self.cmndf[tau] as f64;
        let c = self.cmndf[tau + 1] as f64;
        let den = a - 2.0 * b + c;
        let shift = if den.abs() > 1e-12 { (0.5 * (a - c) / den).clamp(-0.5, 0.5) } else { 0.0 };
        Some((tau as f64 + shift, self.cmndf[tau]))
    }
}

pub struct SoloistEngine {
    core: Option<EngineCore>,
    yin: Option<Yin>,
    sched: Option<Scheduler>,
    detector: Option<OnsetDetector>,
    mono: Vec<f32>,
    grid: i64,
    cache: [Option<PitchEst>; 4],
    voiced: bool,
    last_period: f64,
    /// Next output mark (absolute output frame, fractional).
    mark: f64,
    /// Source centre of the previous voiced grain.
    prev_c: Option<f64>,
    prev_period: f64,
    /// Absolute output frame of the accumulator's write head.
    fin: i64,
    /// Largest grain half-length, output frames.
    h_max: usize,
    unvoiced_half: usize,
    p_cur: f64,
    f_cur: f64,
    glide: f64,
    started: bool,
}

impl Default for SoloistEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SoloistEngine {
    pub fn new() -> Self {
        Self {
            core: None,
            yin: None,
            sched: None,
            detector: None,
            mono: Vec::new(),
            grid: 1,
            cache: [None; 4],
            voiced: false,
            last_period: 0.0,
            mark: 0.0,
            prev_c: None,
            prev_period: 0.0,
            fin: 0,
            h_max: 0,
            unvoiced_half: 0,
            p_cur: 1.0,
            f_cur: 1.0,
            glide: 1.0,
            started: false,
        }
    }

    pub const CAPABILITY: Capability = Capability {
        name: "soloist",
        min_ratio: 0.25,
        max_ratio: 4.0,
        independent_pitch: true,
        formant_control: true,
        realtime_safe: true,
    };

    fn yin_w(&self) -> usize {
        self.yin.as_ref().map(|y| y.w).unwrap_or(0)
    }

    /// Source span one mark may touch around centre `c`.
    fn span(&self, c: f64) -> (i64, i64) {
        let w = self.yin_w() as i64;
        let reach = self.h_max as i64 * 4 + 8;
        let lo = c.floor() as i64 - w / 2 - reach;
        let hi = c.ceil() as i64 + w / 2 + reach;
        (lo, hi)
    }

    /// Period at source position `c` (0 = unvoiced), from the grid cache.
    fn period_at(&mut self, c: f64) -> f64 {
        let g = (c / self.grid as f64).round() as i64 * self.grid;
        for e in self.cache.iter().flatten() {
            if e.grid == g {
                return e.period;
            }
        }
        let core = self.core.as_ref().expect("prepared");
        let yin = self.yin.as_mut().expect("prepared");
        let w = yin.w;
        let ch = core.cfg.channels;
        let from = g - w as i64 / 2;
        let norm = 1.0 / ch as f32;
        for i in 0..w {
            let mut s = 0.0f32;
            for c in 0..ch {
                s += core.ring.at(c, from + i as i64);
            }
            self.mono[i] = s * norm;
        }
        let est = yin.estimate(&self.mono);
        let period = match est {
            Some((tau, ap)) => {
                let thr = if self.voiced { 0.35 } else { 0.2 };
                if ap < thr {
                    // Octave continuity: prefer the candidate near the last
                    // period when this one is a clean multiple of it.
                    let mut t = tau;
                    if self.last_period > 0.0 {
                        let r = t / self.last_period;
                        if (r - 2.0).abs() < 0.08 {
                            t *= 0.5;
                        } else if (r - 0.5).abs() < 0.04 {
                            t *= 2.0;
                        }
                    }
                    t
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        self.voiced = period > 0.0;
        if period > 0.0 {
            self.last_period = period;
        }
        self.cache.rotate_right(1);
        self.cache[0] = Some(PitchEst { grid: g, period });
        period
    }

    /// Waveform-similarity refinement: the position near `guess` whose
    /// surroundings best match the surroundings of `from`, over one period.
    fn refine(&self, from: f64, guess: f64, period: f64) -> f64 {
        let core = self.core.as_ref().expect("prepared");
        let ch = core.cfg.channels;
        let span = (period * 0.125).ceil() as i64;
        let len = period.round() as i64;
        let base = from.round() as i64;
        let g = guess.round() as i64;
        let score = |cand: i64| -> f64 {
            let mut num = 0.0f64;
            let mut ea = 0.0f64;
            let mut eb = 0.0f64;
            let mut j = -len;
            while j < len {
                for c in 0..ch {
                    let a = core.ring.at(c, base + j) as f64;
                    let b = core.ring.at(c, cand + j) as f64;
                    num += a * b;
                    ea += a * a;
                    eb += b * b;
                }
                j += 2;
            }
            if ea <= 1e-12 || eb <= 1e-12 { 0.0 } else { num / (ea * eb).sqrt() }
        };
        // coarse pass on every other lag, then the winner's neighbours
        let step = if span > 16 { 2 } else { 1 };
        let mut best = g;
        let mut best_score = f64::NEG_INFINITY;
        let mut cand = g - span;
        while cand <= g + span {
            let s = score(cand);
            if s > best_score {
                best_score = s;
                best = cand;
            }
            cand += step;
        }
        if step > 1 {
            for cand in [best - 1, best + 1] {
                let s = score(cand);
                if s > best_score {
                    best_score = s;
                    best = cand;
                }
            }
        }
        best as f64
    }

    pub fn retarget(&mut self, map: &mut TimeMap, pitch: f64, formant: FormantPolicy) -> u64 {
        let (Some(core), Some(sched)) = (self.core.as_mut(), self.sched.as_mut()) else {
            return 0;
        };
        let centre = self.mark.round() as i64;
        let offset = sched.retarget(map, &core.cfg.map, centre as f64);
        std::mem::swap(&mut core.cfg.map, map);
        core.cfg.pitch = pitch;
        core.cfg.formant = formant;
        self.mark += offset as f64;
        self.fin += offset;
        let total = core.cfg.map.output_frames().get() as i64;
        core.delivered = (core.delivered as i64 + offset).clamp(0, total.max(0)) as u64;
        core.synth_pos = self.fin.clamp(0, total.max(0)) as u64;
        core.delivered
    }

    fn start_at(&mut self, output_frame: u64) {
        let h = self.h_max as i64;
        self.mark = output_frame as f64;
        self.fin = output_frame as i64 - h;
        self.prev_c = None;
        self.cache = [None; 4];
        self.voiced = false;
        self.last_period = 0.0;
        self.started = false;
        let core = self.core.as_mut().expect("prepared");
        let sched = self.sched.as_mut().expect("prepared");
        sched.reset();
        sched.start_at(output_frame as f64);
        let c = sched.position(&core.cfg.map, output_frame as f64) as f64;
        let w = self.yin.as_ref().map(|y| y.w).unwrap_or(0) as i64;
        let start = (c.floor() as i64 - w / 2 - h * 4 - 8).max(0) as u64;
        core.ring.reset(start);
        core.out.reset();
        core.out.skip(h as usize);
        if let Some(d) = self.detector.as_mut() {
            d.reset(start);
        }
    }
}

#[inline]
fn hann(x: f64) -> f64 {
    // x in -1..1
    if x <= -1.0 || x >= 1.0 {
        0.0
    } else {
        0.5 + 0.5 * (PI * x).cos()
    }
}

#[inline]
fn cubic(ring: &crate::runtime::InputRing, c: usize, pos: f64) -> f32 {
    let i = pos.floor() as i64;
    let f = (pos - i as f64) as f32;
    if f == 0.0 {
        return ring.at(c, i);
    }
    let p0 = ring.at(c, i - 1);
    let p1 = ring.at(c, i);
    let p2 = ring.at(c, i + 1);
    let p3 = ring.at(c, i + 2);
    p1 + 0.5 * f * (p2 - p0 + f * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + f * (3.0 * (p1 - p2) + p3 - p0)))
}

impl Stepper for SoloistEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    fn step(&mut self) -> bool {
        let total = self.core_ref().total_output() as i64;
        let h_max = self.h_max as i64;
        if self.fin >= total {
            return false;
        }
        if self.core_ref().out.space() < 4 * self.h_max + 8 {
            return false;
        }
        let src_len = self.core_ref().cfg.source_frames() as i64;
        // Grain centres sit on whole output frames; the fractional spacing
        // accumulates in `mark`.
        let t = self.mark.round();
        let centre = t as i64;

        // onsets up to the scheduler's horizon
        {
            let core = self.core.as_mut().expect("prepared");
            let det = self.detector.as_mut().expect("prepared");
            let sched = self.sched.as_mut().expect("prepared");
            det.scan(&core.ring, src_len);
            let horizon = sched.detect_horizon(&core.cfg.map, centre as f64);
            if det.position() < horizon.min(src_len) && !core.eof {
                return false;
            }
            while let Some(o) = det.pop() {
                sched.offer_onset(&core.cfg.map, o, centre as f64);
            }
        }
        let ideal = {
            let core = self.core.as_ref().expect("prepared");
            let sched = self.sched.as_ref().expect("prepared");
            sched.position(&core.cfg.map, centre as f64) as f64
        };
        let pc = self.prev_c.unwrap_or(ideal);
        let (lo, _) = self.span(pc.min(ideal));
        let (_, hi) = self.span(pc.max(ideal));
        if !self.core_ref().source_ready(lo, hi) {
            return false;
        }

        // pitch and formant targets
        {
            let cfg = &self.core_ref().cfg;
            let p_t = cfg.pitch.max(1e-3);
            let f_t = cfg.formant.factor(p_t).max(1e-3);
            if !self.started {
                self.p_cur = p_t;
                self.f_cur = f_t;
                self.started = true;
            } else {
                self.p_cur *= (p_t / self.p_cur).powf(self.glide);
                self.f_cur *= (f_t / self.f_cur).powf(self.glide);
            }
        }
        let p = self.p_cur;
        let f = self.f_cur;

        let period = self.period_at(ideal);
        let (c, half_src, spacing) = if period > 0.0 {
            let c = match self.prev_c {
                Some(pc) if self.prev_period > 0.0 => {
                    // Continue from the previous grain by the whole number of
                    // periods that stays closest to the map, then pin the
                    // exact lag by waveform similarity.
                    let tp = 0.5 * (period + self.prev_period);
                    let n = ((ideal - pc) / tp).round();
                    if n == 0.0 { pc } else { self.refine(pc, pc + n * tp, tp) }
                }
                _ => ideal.round(),
            };
            // grain: two source periods, at least 1.5 output spacings
            let spacing = period / p;
            let half = period.max(0.75 * spacing * f);
            (c, half, spacing)
        } else {
            let half = self.unvoiced_half as f64;
            (ideal.round(), half, half)
        };
        // Grain length in output frames: the source span read at rate f.
        let half_out = (half_src / f).min(h_max as f64 - 1.0).max(4.0);

        // Overlap-add the grain, window-normalised.
        {
            let core = self.core.as_mut().expect("prepared");
            let ch = core.cfg.channels;
            let start = (t - half_out).ceil() as i64;
            let end = (t + half_out).floor() as i64;
            for o in start..=end {
                let off = o - self.fin;
                if off < 0 {
                    continue;
                }
                let x = (o as f64 - t) / half_out;
                let w = hann(x) as f32;
                if w <= 0.0 {
                    continue;
                }
                let pos = c + (o as f64 - t) * f;
                for cc in 0..ch {
                    let v = if f == 1.0 {
                        core.ring.at(cc, pos as i64)
                    } else {
                        cubic(&core.ring, cc, pos)
                    };
                    core.out.add(cc, off as usize, v * w);
                }
                core.out.add_norm(off as usize, w);
            }
        }

        if period > 0.0 {
            self.prev_c = Some(c);
            self.prev_period = period;
        } else {
            self.prev_c = None;
            self.prev_period = 0.0;
        }
        self.mark = t + spacing.max(1.0);

        // Finalise everything no future grain can reach.
        let new_fin = ((self.mark - h_max as f64).floor() as i64).min(total);
        if new_fin > self.fin {
            let n = (new_fin - self.fin) as usize;
            let core = self.core.as_mut().expect("prepared");
            core.out.advance_normalized(n, 1e-3);
            self.fin = new_fin;
            core.synth_pos = self.fin.clamp(0, total) as u64;
            // everything older than the span of the next grain can go
            let (keep, _) = self.span(self.prev_c.unwrap_or(c).min(c));
            let core = self.core.as_mut().expect("prepared");
            core.ring.discard_before(keep.max(0) as u64);
        }
        true
    }
}

impl StretchEngine for SoloistEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        let (lo, hi) = cfg.map.ratio_range();
        let cap = Self::CAPABILITY;
        if cfg.map.source_frames().get() > 0 && (lo < cap.min_ratio || hi > cap.max_ratio) {
            return Err(PrepareError::UnsupportedRatio {
                requested: if lo < cap.min_ratio { lo } else { hi },
                min: cap.min_ratio,
                max: cap.max_ratio,
            });
        }
        if !(0.25..=4.0).contains(&cfg.pitch) {
            return Err(PrepareError::UnsupportedPitch { requested: cfg.pitch, min: 0.25, max: 4.0 });
        }
        let rate = cfg.sample_rate as f64;
        let yin = Yin::new(cfg.sample_rate);
        // Longest grain: two periods of the lowest note, read at the lowest
        // formant rate, stretched for the lowest pitch.
        self.h_max = (2.0 * rate / MIN_HZ * 2.0).ceil() as usize;
        self.unvoiced_half = (rate * UNVOICED_HALF).round() as usize;
        self.grid = ((rate * 0.005).round() as i64).max(16);
        let lock_n = ((rate * 0.02).round() as usize).next_power_of_two();
        let sched = Scheduler::new(lock_n, lock_n / 8, cfg.sample_rate, 1.0, 2.0, cfg.transient_protect);
        let speed = (1.0 / lo.max(1e-3)).max(4.0);
        let ring = yin.w + self.h_max * 16 + sched.lookahead_output(speed) + cfg.max_block * 2 + 4096;
        let out = self.h_max * 8 + cfg.max_block * 2 + 8192;
        self.mono = vec![0.0; yin.w];
        self.yin = Some(yin);
        self.sched = Some(sched);
        self.detector = Some(OnsetDetector::new(cfg.channels, cfg.sample_rate));
        self.core = Some(EngineCore::new(cfg.clone(), ring, out));
        {
            let core = self.core.as_mut().expect("prepared");
            core.out = crate::runtime::OutputAccum::with_norm(cfg.channels, out);
        }
        self.glide = 1.0 - (-(rate * 0.004) / (GLIDE_SECONDS * rate)).exp();
        self.start_at(0);
        let core = self.core.as_mut().expect("prepared");
        core.synth_pos = 0;
        core.delivered = 0;
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        if self.core.is_none() {
            return;
        }
        let total = self.core_ref().total_output();
        let target = position.output_frame.min(total);
        self.core.as_mut().expect("prepared").eof = false;
        self.start_at(target);
        let core = self.core.as_mut().expect("prepared");
        core.synth_pos = target;
        core.delivered = target;
    }

    fn process(
        &mut self,
        input: AudioView<'_>,
        output: AudioViewMut<'_>,
        end_of_input: bool,
    ) -> Result<ProcessReport, ProcessError> {
        self.drive(input, output, end_of_input)
    }

    fn latency(&self) -> LatencyInfo {
        LatencyInfo {
            lookahead_input_frames: (self.yin_w() / 2 + self.h_max * 4) as u64,
            startup_padding_input_frames: 0,
            presentation_delay_output_frames: self.h_max as u64,
            tail_output_frames: 0,
        }
    }

    fn output_frames(&self) -> u64 {
        self.core.as_ref().map(|c| c.total_output()).unwrap_or(0)
    }

    fn input_position(&self) -> u64 {
        self.core.as_ref().map(|c| c.ring.end()).unwrap_or(0)
    }

    fn retarget(&mut self, map: &mut TimeMap, pitch: f64, formant: FormantPolicy) -> Option<u64> {
        if self.core.is_none() {
            return None;
        }
        Some(SoloistEngine::retarget(self, map, pitch, formant))
    }
}
