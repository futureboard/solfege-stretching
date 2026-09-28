//! Elastic: the general-purpose engine family.
//!
//! Three presets share one spectral kernel ([`spectral`]) and one scheduler
//! ([`schedule`]); they differ in window, overlap and how far the rate may
//! bend to make room for a transient:
//!
//! | preset    | window @48k  | overlap | for |
//! |-----------|--------------|---------|-----|
//! | Pro       | 4096 (85 ms) | 8x      | anything: mixes, keys, pads, vocals with backing |
//! | Efficient | 2048 (43 ms) | 4x      | the same at a quarter of the work |
//! | Rhythmic  | 1024 (21 ms) | 4x      | drums, percussive loops, rhythm parts |
//!
//! ```text
//! source --(scheduler: map + transient locks)--> kernel --> z --(resampler at p)--> output
//! ```
//!
//! Transposition is stretch-then-resample: the kernel stretches by
//! `alpha * p` into an internal timeline `z`, and a band-limited resampler
//! reads `z` back at rate `p`. Both run inside this one engine on one clock,
//! `u(t)` (the `z` position of output frame `t`), which is what lets pitch
//! glide live: changing `p` just changes the slope of `u` from the next
//! block on, and the scheduler asks where the kernel's next frame falls in
//! output time through the same `u`. A transient therefore passes through
//! the kernel at unity whatever the transposition, and the resampler - which
//! shifts time and pitch together, exactly - does the rest.
//!
//! A running engine can also be retargeted - new map, new pitch, new formant
//! policy - without being rebuilt ([`ElasticEngine::retarget`]).

pub mod schedule;
pub mod spectral;

use super::core::{EngineCore, Stepper};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::document::FormantPolicy;
use crate::dsp::resample::{SincBank, DEFAULT_HALF_TAPS, DEFAULT_OVERSAMPLE};
use crate::mapping::TimeMap;
use schedule::{OnsetDetector, Scheduler};
use spectral::{FrameParams, SpectralKernel};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ElasticPreset {
    Pro,
    Efficient,
    Rhythmic,
}

impl ElasticPreset {
    /// Window length in seconds; rounded to a power of two in frames.
    fn window_seconds(self) -> f64 {
        match self {
            ElasticPreset::Pro => 0.0853,
            ElasticPreset::Efficient => 0.0427,
            ElasticPreset::Rhythmic => 0.0213,
        }
    }
    fn overlap(self) -> usize {
        match self {
            ElasticPreset::Pro => 8,
            ElasticPreset::Efficient | ElasticPreset::Rhythmic => 4,
        }
    }
    /// How far the read rate may deviate from the map while it makes room
    /// for a locked transient (multiplicative).
    fn max_rate_deviation(self) -> f64 {
        match self {
            ElasticPreset::Pro | ElasticPreset::Efficient => 1.6,
            ElasticPreset::Rhythmic => 2.5,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ElasticPreset::Pro => "elastic pro",
            ElasticPreset::Efficient => "elastic efficient",
            ElasticPreset::Rhythmic => "rhythmic",
        }
    }
    pub fn window_frames(self, sample_rate: u32) -> usize {
        ((sample_rate as f64 * self.window_seconds()).round() as usize)
            .next_power_of_two()
            .clamp(256, 16384)
    }
}

/// Time constant for pitch and formant glides, seconds.
const GLIDE_SECONDS: f64 = 0.035;
/// Output frames between glide updates. Fixed, so the result does not depend
/// on the caller's block size.
const GLIDE_BLOCK: i64 = 64;
/// Highest transposition the resampler is built for (two octaves).
const MAX_PITCH: f64 = 4.0;

/// The kernel's output timeline, overlap-added in place and read back by the
/// resampler. Indexed by absolute `z` frame.
struct ZBuf {
    planes: Vec<Vec<f32>>,
    /// Absolute index of `planes[c][0]`.
    start: i64,
}

impl ZBuf {
    fn new(channels: usize, cap: usize) -> Self {
        Self { planes: vec![vec![0.0; cap]; channels], start: 0 }
    }
    fn reset(&mut self, start: i64) {
        self.start = start;
        for p in &mut self.planes {
            p.fill(0.0);
        }
    }
    #[inline]
    fn add(&mut self, c: usize, abs: i64, v: f32) {
        let i = abs - self.start;
        if i >= 0 && (i as usize) < self.planes[c].len() {
            self.planes[c][i as usize] += v;
        }
    }
    #[inline]
    fn at(&self, c: usize, abs: i64) -> f32 {
        let i = abs - self.start;
        if i >= 0 && (i as usize) < self.planes[c].len() {
            self.planes[c][i as usize]
        } else {
            0.0
        }
    }
    fn discard_before(&mut self, abs: i64) {
        let d = abs - self.start;
        if d <= 0 {
            return;
        }
        let cap = self.planes[0].len();
        let d = (d as usize).min(cap);
        for p in &mut self.planes {
            p.copy_within(d.., 0);
            p[cap - d..].fill(0.0);
        }
        self.start += d as i64;
    }
}

pub struct ElasticEngine {
    preset: ElasticPreset,
    core: Option<EngineCore>,
    kernel: Option<SpectralKernel>,
    sched: Option<Scheduler>,
    detector: Option<OnsetDetector>,
    bank: Option<SincBank>,
    z: ZBuf,
    n: usize,
    hop: usize,
    ctx: i64,
    /// Start of the next kernel frame's window in `z`; everything before it
    /// is final.
    zfin: i64,
    /// Next output frame to produce.
    t_next: i64,
    /// Clock: `u(t) = u_base + (t - t_base) * p_cur`.
    t_base: i64,
    u_base: f64,
    p_cur: f64,
    f_cur: f64,
    glide: f64,
    a_prev: Option<i64>,
}

impl ElasticEngine {
    pub fn new(preset: ElasticPreset) -> Self {
        Self {
            preset,
            core: None,
            kernel: None,
            sched: None,
            detector: None,
            bank: None,
            z: ZBuf::new(1, 1),
            n: 0,
            hop: 0,
            ctx: 0,
            zfin: 0,
            t_next: 0,
            t_base: 0,
            u_base: 0.0,
            p_cur: 1.0,
            f_cur: 1.0,
            glide: 1.0,
            a_prev: None,
        }
    }

    pub const CAPABILITY: Capability = Capability {
        name: "elastic",
        min_ratio: 0.05,
        max_ratio: 20.0,
        independent_pitch: true,
        formant_control: true,
        realtime_safe: true,
    };

    pub fn preset(&self) -> ElasticPreset {
        self.preset
    }

    /// Window and hop actually in use, once prepared.
    pub fn geometry(&self) -> (usize, usize) {
        (self.n, self.hop)
    }

    /// Output time of `z` position `u`, by the current clock.
    #[inline]
    fn t_of(&self, u: f64) -> f64 {
        self.t_base as f64 + (u - self.u_base) / self.p_cur
    }

    /// Frame grid in `z`: centres at `-n/2 + hop + m*hop`. The first frame
    /// whose window reaches `need`; returns its window start.
    fn window_start_for(&self, need: i64) -> i64 {
        let n = self.n as i64;
        let h = self.hop as i64;
        let m0 = (need - h).div_euclid(h) + 1;
        -n / 2 + h + m0 * h - n / 2
    }

    /// Put the clock, the `z` timeline and the input ring at output frame `t`.
    fn start_at(&mut self, t: i64) {
        let (p, f) = {
            let cfg = &self.core.as_ref().expect("prepared").cfg;
            let p = cfg.pitch.clamp(1.0 / MAX_PITCH, MAX_PITCH);
            (p, cfg.formant.factor(p))
        };
        self.p_cur = p;
        self.f_cur = f;
        self.t_next = t;
        self.t_base = t;
        self.u_base = p * t as f64;
        let need = self.u_base.floor() as i64 - self.ctx - 1;
        self.zfin = self.window_start_for(need);
        self.z.reset(self.zfin);
        self.a_prev = None;
        if let Some(k) = self.kernel.as_mut() {
            k.reset();
        }
        let centre_t = self.t_of((self.zfin + self.n as i64 / 2) as f64);
        let n = self.n as i64;
        let sched = self.sched.as_mut().expect("prepared");
        sched.reset();
        sched.set_unity(p);
        sched.start_at(t as f64);
        let core = self.core.as_mut().expect("prepared");
        let a = sched.position(&core.cfg.map, centre_t);
        let start = (a - n / 2).max(0) as u64;
        core.reset_to(start);
        core.synth_pos = t as u64;
        core.delivered = t as u64;
        if let Some(d) = self.detector.as_mut() {
            d.reset(start);
        }
    }

    /// Swap in a new map, pitch and formant without rebuilding anything.
    ///
    /// The engine keeps its phase state, its `z` timeline and its source
    /// position, so the change is heard within a block and without a seam;
    /// a pitch change glides. Output frame numbers are relabelled into the
    /// new map's coordinates and the new logical position of the next
    /// delivered frame is returned. `map` receives the old map, so the caller
    /// can dispose of it away from the audio thread. Allocation-free.
    pub fn retarget(&mut self, map: &mut TimeMap, pitch: f64, formant: FormantPolicy) -> u64 {
        let (Some(core), Some(sched)) = (self.core.as_mut(), self.sched.as_mut()) else {
            return 0;
        };
        let offset = sched.retarget(map, &core.cfg.map, self.t_next as f64);
        std::mem::swap(&mut core.cfg.map, map);
        core.cfg.pitch = pitch.clamp(1.0 / MAX_PITCH, MAX_PITCH);
        core.cfg.formant = formant;
        self.t_next += offset;
        self.t_base += offset;
        let total = core.cfg.map.output_frames().get() as i64;
        core.delivered = (core.delivered as i64 + offset).clamp(0, total.max(0)) as u64;
        core.synth_pos = self.t_next.clamp(0, total.max(0)) as u64;
        core.delivered
    }

    /// Resample as much output as the finished part of `z` allows. Returns
    /// frames produced.
    fn produce(&mut self) -> usize {
        let core = self.core.as_mut().expect("prepared");
        let bank = self.bank.as_ref().expect("prepared");
        let total = core.cfg.map.output_frames().get() as i64;
        let ch = core.cfg.channels;
        let room = core.out.space();
        let mut made = 0usize;
        while self.t_next < total && made < room {
            // Glide the transposition on a fixed grid of output frames.
            if self.t_next.rem_euclid(GLIDE_BLOCK) == 0 {
                let p_t = core.cfg.pitch.clamp(1.0 / MAX_PITCH, MAX_PITCH);
                let f_t = core.cfg.formant.factor(p_t);
                if self.p_cur != p_t {
                    let u_now = self.u_base + (self.t_next - self.t_base) as f64 * self.p_cur;
                    let mut p = self.p_cur * (p_t / self.p_cur).powf(self.glide);
                    if (p / p_t - 1.0).abs() < 1e-5 {
                        p = p_t;
                    }
                    self.u_base = u_now;
                    self.t_base = self.t_next;
                    self.p_cur = p;
                }
                if self.f_cur != f_t {
                    let mut f = self.f_cur * (f_t / self.f_cur).powf(self.glide);
                    if (f / f_t - 1.0).abs() < 1e-5 {
                        f = f_t;
                    }
                    self.f_cur = f;
                }
            }
            let u = self.u_base + (self.t_next - self.t_base) as f64 * self.p_cur;
            let exact = self.p_cur == 1.0 && u.fract() == 0.0;
            let need = if exact { u as i64 + 1 } else { u.floor() as i64 + self.ctx + 1 };
            if need > self.zfin {
                break;
            }
            if exact {
                for c in 0..ch {
                    core.out.add(c, made, self.z.at(c, u as i64));
                }
            } else {
                let cutoff = (1.0 / self.p_cur).min(1.0) as f32;
                let local = u - self.z.start as f64;
                for c in 0..ch {
                    let v = bank.read(&self.z.planes[c], local, cutoff);
                    core.out.add(c, made, v);
                }
            }
            made += 1;
            self.t_next += 1;
        }
        if made > 0 {
            core.out.advance(made);
            core.synth_pos = self.t_next.clamp(0, total) as u64;
            let u = self.u_base + (self.t_next - self.t_base) as f64 * self.p_cur;
            self.z.discard_before(u.floor() as i64 - self.ctx - 2);
        }
        made
    }

    /// Run one kernel frame into `z`. Returns false when it has to wait for
    /// input (or detection) first.
    fn synthesize(&mut self) -> bool {
        let n = self.n as i64;
        let hop = self.hop as i64;
        let centre_z = self.zfin + n / 2;
        let t_c = self.t_of(centre_z as f64);
        let core = self.core.as_mut().expect("prepared");
        let kernel = self.kernel.as_mut().expect("prepared");
        let sched = self.sched.as_mut().expect("prepared");
        let det = self.detector.as_mut().expect("prepared");
        let src_len = core.cfg.source_frames() as i64;

        // Transient detection runs over whatever the ring holds; the
        // scheduler needs it complete up to its horizon before it commits.
        det.scan(&core.ring, src_len);
        let horizon = sched.detect_horizon(&core.cfg.map, t_c);
        if det.position() < horizon.min(src_len) && !core.eof {
            return false;
        }
        sched.set_unity(self.p_cur);
        while let Some(o) = det.pop() {
            sched.offer_onset(&core.cfg.map, o, t_c);
        }

        let mut a = sched.position(&core.cfg.map, t_c);
        if let Some(prev) = self.a_prev {
            a = a.max(prev);
        }
        let from = a - n / 2;
        if !core.source_ready(from, from + n) {
            return false;
        }

        let params = FrameParams {
            analysis_hop: self.a_prev.map(|p| a - p),
            envelope_shift: (self.p_cur / self.f_cur) as f32,
            transient: sched.transient_in(a),
        };
        let ring = &core.ring;
        let z = &mut self.z;
        let zw = self.zfin;
        kernel.frame(
            params,
            |c, i| ring.at(c, from + i as i64),
            |c, i, v| z.add(c, zw + i as i64, v),
        );
        self.a_prev = Some(a);
        self.zfin += hop;
        core.ring.discard_before(from.max(0) as u64);
        true
    }
}

impl Stepper for ElasticEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    fn step(&mut self) -> bool {
        let total = self.core_ref().total_output() as i64;
        if self.t_next >= total {
            return false;
        }
        if self.produce() > 0 {
            return true;
        }
        if self.core_ref().out.space() == 0 {
            return false;
        }
        self.synthesize()
    }
}

impl StretchEngine for ElasticEngine {
    fn capability(&self) -> Capability {
        Capability { name: self.preset.label(), ..Self::CAPABILITY }
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
        if !(1.0 / MAX_PITCH..=MAX_PITCH).contains(&cfg.pitch) {
            return Err(PrepareError::UnsupportedPitch {
                requested: cfg.pitch,
                min: 1.0 / MAX_PITCH,
                max: MAX_PITCH,
            });
        }
        let n = cfg
            .stft_size
            .filter(|s| s.is_power_of_two() && *s >= 256)
            .unwrap_or_else(|| self.preset.window_frames(cfg.sample_rate));
        let hop = n / self.preset.overlap();
        self.n = n;
        self.hop = hop;

        let sched = Scheduler::new(
            n,
            hop,
            cfg.sample_rate,
            1.0,
            self.preset.max_rate_deviation(),
            cfg.transient_protect,
        );
        let bank = SincBank::new(DEFAULT_HALF_TAPS, DEFAULT_OVERSAMPLE, MAX_PITCH);
        self.ctx = bank.context() as i64;
        // The input ring holds one window plus the scheduler's look-ahead at
        // the fastest read speed a retarget may ask for, plus a caller block.
        let speed = (1.0 / lo.max(1e-3)).max(4.0) * MAX_PITCH;
        let ring = n + sched.lookahead_output(speed) + cfg.max_block * 2 + 4096;
        let out = cfg.max_block * 2 + 4096;
        self.z = ZBuf::new(cfg.channels, 2 * n + 4 * self.ctx as usize + 4096);
        self.kernel = Some(SpectralKernel::new(n, hop, cfg.channels, cfg.sample_rate));
        self.sched = Some(sched);
        self.detector = Some(OnsetDetector::new(cfg.channels, cfg.sample_rate));
        self.bank = Some(bank);
        self.glide =
            1.0 - (-(GLIDE_BLOCK as f64) / (GLIDE_SECONDS * cfg.sample_rate as f64)).exp();
        self.core = Some(EngineCore::new(cfg.clone(), ring, out));
        self.start_at(0);
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        if self.core.is_none() {
            return;
        }
        let total = self.core_ref().total_output();
        self.start_at(position.output_frame.min(total) as i64);
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
        let look = self.sched.as_ref().map(|s| s.lookahead_output(1.0)).unwrap_or(0);
        LatencyInfo {
            lookahead_input_frames: (self.n / 2 + look) as u64,
            startup_padding_input_frames: 0,
            presentation_delay_output_frames: (self.n / 2 + self.ctx as usize) as u64,
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
        Some(ElasticEngine::retarget(self, map, pitch, formant))
    }
}
