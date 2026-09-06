//! Low-frequency specialised path.
//!
//! Below roughly 200 Hz a phase vocoder sized for the whole spectrum is working
//! blind. At 48 kHz a 2048-point window puts a 55 Hz fundamental and its second
//! harmonic 2.3 bins apart, so the instantaneous-frequency estimate for each is
//! contaminated by the other, the two drift against each other frame by frame,
//! and the result is the chorusing, phasing and robotic edge that a stretched
//! bass line is notorious for. The window that fixes it — 16384 points, 341 ms —
//! is far too long for the rest of the spectrum, where it would turn every
//! attack to mush.
//!
//! So the bass gets its own analysis. This module is a second STFT, an order of
//! magnitude longer than the main one, that handles only the bins below the
//! crossover; the main engine handles everything above it, and the two are
//! summed. The split is spectral and amplitude-complementary — the weights add
//! to one at every frequency — so with no stretching at all the two halves
//! reconstruct the input exactly.
//!
//! Three things make the low band behave where a plain phase vocoder does not:
//!
//! * **Fundamental tracking.** At 2.9 Hz per bin a bass fundamental is a
//!   resolved peak, so it can be found, refined to a fraction of a bin, checked
//!   against its own octave, and followed across frames.
//! * **Harmonic phase locking anchored on F0.** Every harmonic advances by
//!   `h * omega_F0 * Hs` rather than by its own noisy per-bin estimate. That is
//!   what stops the harmonics beating against each other: they are rigidly
//!   locked to one trajectory, which is exactly what they are in the source.
//! * **A single shared trajectory for both channels.** The frequency estimate
//!   comes from Mid, and both channels are advanced by the same angle, so the
//!   inter-channel phase relationship in the bass cannot drift. Nothing here is
//!   ever decided per channel (system-design.md sec.9).

use crate::dsp::stft::{principal_arg, RealStft};
use crate::dsp::window::sqrt_hann_periodic;
use crate::mapping::TimeMap;
use crate::runtime::{InputRing, OutputAccum};
use realfft::num_complex::Complex32;

/// Highest harmonic the tracker will lock. Above the crossover there is nothing
/// left for it to lock anyway.
const MAX_HARMONICS: usize = 8;
/// Lowest fundamental worth looking for. Below this it is rumble, not a note.
const F0_MIN_HZ: f64 = 25.0;
/// A fundamental is only trusted when its peak stands this far above the median
/// of the band.
const F0_PROMINENCE: f32 = 4.0;
/// How far the tracked fundamental may jump between frames while it is
/// confident, as a ratio. A bass line moves by semitones, not by octaves,
/// between two frames 85 ms apart.
const F0_MAX_JUMP: f64 = 1.06;

pub struct LowBandConfig {
    pub sample_rate: u32,
    pub channels: usize,
    /// STFT size. Must be a power of two.
    pub n: usize,
    /// Below this the low band is alone.
    pub crossover_lo_hz: f64,
    /// Above this the main engine is alone.
    pub crossover_hi_hz: f64,
    pub max_block: usize,
}

pub struct LowBand {
    stft: RealStft,
    n: usize,
    hop: usize,
    bins: usize,
    lead: usize,
    channels: usize,
    sample_rate: u32,
    window: Vec<f32>,
    /// Own overlap-add, merged with the main band's by the caller.
    pub acc: OutputAccum,
    /// Highest bin this band contributes to; everything above is the main
    /// engine's and is never even looked at.
    top_bin: usize,
    /// Crossover weight on this band's grid.
    weight: Vec<f32>,

    spec: Vec<Vec<Complex32>>,
    mag: Vec<Vec<f32>>,
    cur_phase: Vec<Vec<f32>>,
    prev_phase: Vec<Vec<f32>>,
    theta: Vec<Vec<f32>>,

    /// Mid magnitude and phase: the reference everything is decided from.
    mid_mag: Vec<f32>,
    mid_phase: Vec<f32>,
    mid_prev_phase: Vec<f32>,
    /// Per-bin phase advance for this frame, shared by every channel.
    delta: Vec<f32>,
    /// Which harmonic a bin belongs to, or `None` for the noise floor between.
    harmonic_of: Vec<Option<usize>>,
    /// Anchor bin of each harmonic.
    anchor: Vec<usize>,
    median_scratch: Vec<f32>,
    time_in: Vec<f32>,
    time_out: Vec<f32>,

    /// Tracked fundamental, in fractional bins. Zero when not tracking.
    f0_bin: f64,
    f0_confident: bool,
    /// Angular frequency of the fundamental, radians per sample.
    omega_f0: f32,

    raw_produced: u64,
    prev_source: f64,
    first_frame: bool,
    /// Diagnostics.
    tracked_frames: u64,
    total_frames: u64,
}

impl LowBand {
    pub fn new(cfg: &LowBandConfig) -> Self {
        let n = cfg.n;
        let hop = n / 4;
        let bins = n / 2 + 1;
        let ch = cfg.channels;
        let hz_per_bin = cfg.sample_rate as f64 / n as f64;
        let top_bin = ((cfg.crossover_hi_hz / hz_per_bin).ceil() as usize + 2).min(bins - 1);

        let weight = (0..bins)
            .map(|k| {
                crossover_weight(
                    k as f64 * hz_per_bin,
                    cfg.crossover_lo_hz,
                    cfg.crossover_hi_hz,
                )
            })
            .collect();

        // The accumulator has to hold a whole window plus a hop plus whatever
        // the caller has not merged yet.
        let cap = (n * 3 + cfg.max_block).max(8192);
        let mut acc = OutputAccum::with_norm(ch, cap);
        let lead = n - hop;
        acc.skip(lead);

        Self {
            stft: RealStft::new(n),
            n,
            hop,
            bins,
            lead,
            channels: ch,
            sample_rate: cfg.sample_rate,
            window: sqrt_hann_periodic(n),
            acc,
            top_bin,
            weight,
            spec: vec![vec![Complex32::new(0.0, 0.0); bins]; ch],
            mag: vec![vec![0.0; bins]; ch],
            cur_phase: vec![vec![0.0; bins]; ch],
            prev_phase: vec![vec![0.0; bins]; ch],
            theta: vec![vec![0.0; bins]; ch],
            mid_mag: vec![0.0; bins],
            mid_phase: vec![0.0; bins],
            mid_prev_phase: vec![0.0; bins],
            delta: vec![0.0; bins],
            harmonic_of: vec![None; bins],
            anchor: vec![0; MAX_HARMONICS + 1],
            median_scratch: vec![0.0; bins],
            time_in: vec![0.0; n],
            time_out: vec![0.0; n],
            f0_bin: 0.0,
            f0_confident: false,
            omega_f0: 0.0,
            raw_produced: 0,
            prev_source: 0.0,
            first_frame: true,
            tracked_frames: 0,
            total_frames: 0,
        }
    }

    pub fn window_size(&self) -> usize {
        self.n
    }
    pub fn lead(&self) -> usize {
        self.lead
    }
    /// Share of frames where a fundamental was confidently tracked.
    pub fn tracking_ratio(&self) -> f64 {
        if self.total_frames == 0 {
            0.0
        } else {
            self.tracked_frames as f64 / self.total_frames as f64
        }
    }
    /// The last tracked fundamental in Hz, or `None`.
    pub fn tracked_hz(&self) -> Option<f64> {
        if self.f0_confident {
            Some(self.f0_bin * self.sample_rate as f64 / self.n as f64)
        } else {
            None
        }
    }

    pub fn reset(&mut self, output_frame: u64) {
        self.acc.reset();
        self.acc.skip(self.lead);
        self.raw_produced = output_frame;
        self.prev_source = 0.0;
        self.first_frame = true;
        self.f0_bin = 0.0;
        self.f0_confident = false;
        self.omega_f0 = 0.0;
        for c in 0..self.channels {
            self.prev_phase[c].fill(0.0);
            self.cur_phase[c].fill(0.0);
            self.theta[c].fill(0.0);
        }
        self.mid_prev_phase.fill(0.0);
    }

    /// Output frames this band has finished and not yet handed over.
    pub fn ready(&self) -> usize {
        self.acc.ready()
    }

    /// Source position the next frame will read from.
    pub fn next_source(&self, map: &TimeMap) -> f64 {
        analysis_pos(map, self.raw_produced as f64 - self.lead as f64)
    }

    /// Synthesise one frame. Returns false when it cannot yet.
    pub fn step(
        &mut self,
        ring: &InputRing,
        map: &TimeMap,
        eof: bool,
        source_frames: u64,
        total_output: u64,
    ) -> bool {
        let n = self.n;
        let hop = self.hop;
        let raw_target = total_output + self.lead as u64;
        if self.raw_produced >= raw_target {
            return false;
        }
        if self.acc.space() < n {
            return false;
        }
        let t = self.raw_produced as f64 - self.lead as f64;
        let s = analysis_pos(map, t);
        let s0 = s.floor() as i64;
        let frac = (s - s0 as f64) as f32;
        if !source_ready(ring, eof, source_frames, s0, s0 + n as i64) {
            return false;
        }

        let mut ha = (s - self.prev_source).max(1e-3);
        if self.first_frame {
            ha = hop as f64 * map.speed_at_output(0.0).max(1e-6);
        }

        // --- analysis, and the Mid reference --------------------------------
        let top = self.top_bin;
        for c in 0..self.channels {
            for i in 0..n {
                self.time_in[i] = ring.at(c, s0 + i as i64) * self.window[i];
            }
            self.stft.forward(&self.time_in, &mut self.spec[c]);
            if frac != 0.0 {
                for k in 0..=top {
                    let a = std::f32::consts::TAU * k as f32 / n as f32 * frac;
                    let (sa, ca) = a.sin_cos();
                    let v = self.spec[c][k];
                    self.spec[c][k] = Complex32::new(v.re * ca - v.im * sa, v.re * sa + v.im * ca);
                }
            }
            for k in 0..=top {
                self.mag[c][k] = self.spec[c][k].norm();
                self.cur_phase[c][k] = self.spec[c][k].arg();
            }
        }
        // Mid is the sum of the channel spectra: the transform is linear, so
        // this is the spectrum of (L+R)/2 without a second FFT.
        for k in 0..=top {
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for c in 0..self.channels {
                re += self.spec[c][k].re;
                im += self.spec[c][k].im;
            }
            let inv = 1.0 / self.channels as f32;
            let m = Complex32::new(re * inv, im * inv);
            self.mid_mag[k] = m.norm();
            self.mid_phase[k] = m.arg();
        }

        // --- per-bin instantaneous frequency, from Mid ----------------------
        let two_pi = std::f32::consts::TAU;
        for k in 0..=top {
            let omega_k = two_pi * k as f32 / n as f32;
            let d = principal_arg(self.mid_phase[k] - self.mid_prev_phase[k] - omega_k * ha as f32);
            self.delta[k] = omega_k + d / ha as f32;
        }

        // --- fundamental tracking -------------------------------------------
        self.total_frames += 1;
        self.track_f0();
        if self.f0_confident {
            self.tracked_frames += 1;
            // The fundamental's own instantaneous frequency, read at its peak
            // bin, is the trajectory every harmonic will ride.
            let kb = self.f0_bin.round() as usize;
            self.omega_f0 = self.delta[kb.min(top)];
        }
        self.assign_harmonics();

        // --- phase advance, shared by every channel -------------------------
        //
        // Anchors move by `h * omega_F0 * Hs`, so the harmonics stay rigidly
        // related instead of each following its own estimate and beating
        // against the others. Bins between harmonics keep their own advance.
        if self.first_frame {
            for c in 0..self.channels {
                for k in 0..=top {
                    self.theta[c][k] = self.cur_phase[c][k];
                }
            }
        } else {
            for k in 0..=top {
                let adv = match self.harmonic_of[k] {
                    Some(h) if self.f0_confident => self.omega_f0 * h as f32,
                    _ => self.delta[k],
                } * hop as f32;
                for c in 0..self.channels {
                    self.theta[c][k] += adv;
                }
            }
            // Identity locking inside each harmonic's region, per channel, from
            // that channel's own analysed offsets - which is what preserves the
            // inter-channel relationship rather than flattening it.
            for k in 0..=top {
                if let Some(h) = self.harmonic_of[k] {
                    let a = self.anchor[h];
                    if a != k {
                        for c in 0..self.channels {
                            let off =
                                principal_arg(self.cur_phase[c][k] - self.cur_phase[c][a]);
                            self.theta[c][k] = self.theta[c][a] + off;
                        }
                    }
                }
            }
        }

        // --- synthesis -------------------------------------------------------
        for c in 0..self.channels {
            for k in 0..self.bins {
                if k > top {
                    self.spec[c][k] = Complex32::new(0.0, 0.0);
                    continue;
                }
                let a = self.mag[c][k] * self.weight[k];
                let th = self.theta[c][k];
                self.spec[c][k] = Complex32::new(a * th.cos(), a * th.sin());
            }
            self.spec[c][0].im = 0.0;
            self.spec[c][self.bins - 1].im = 0.0;
            self.stft.inverse(&self.spec[c], &mut self.time_out);
            for i in 0..n {
                self.acc.add(c, i, self.time_out[i] * self.window[i]);
            }
        }
        for i in 0..n {
            self.acc.add_norm(i, self.window[i] * self.window[i]);
        }

        for c in 0..self.channels {
            self.prev_phase[c][..=top].copy_from_slice(&self.cur_phase[c][..=top]);
        }
        self.mid_prev_phase[..=top].copy_from_slice(&self.mid_phase[..=top]);

        let advance = hop.min((raw_target - self.raw_produced) as usize);
        self.acc.advance_normalized(advance, 1e-4);
        self.raw_produced += advance as u64;
        self.prev_source = s;
        self.first_frame = false;
        true
    }

    /// Find the fundamental: strongest resolved peak in the band, refined to a
    /// fraction of a bin, checked against its own sub-octave, and held to a
    /// plausible step from the last frame.
    fn track_f0(&mut self) {
        let hz_per_bin = self.sample_rate as f64 / self.n as f64;
        let lo = ((F0_MIN_HZ / hz_per_bin).floor() as usize).max(2);
        let hi = self.top_bin.saturating_sub(1);
        if hi <= lo + 2 {
            self.f0_confident = false;
            return;
        }

        let len = hi - lo + 1;
        self.median_scratch[..len].copy_from_slice(&self.mid_mag[lo..=hi]);
        let s = &mut self.median_scratch[..len];
        s.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = s[len / 2].max(1e-9);

        let mut best = lo;
        let mut best_v = 0.0f32;
        for k in lo..=hi {
            let m = self.mid_mag[k];
            if m > best_v && m >= self.mid_mag[k - 1] && m >= self.mid_mag[k + 1] {
                best_v = m;
                best = k;
            }
        }
        if best_v < median * F0_PROMINENCE {
            self.f0_confident = false;
            return;
        }

        // Prefer a sub-octave that is present: a strong second harmonic is easy
        // to mistake for the fundamental, and locking to it would put every
        // real harmonic between two anchors.
        let mut chosen = best;
        let half = best / 2;
        if half >= lo && self.mid_mag[half] > best_v * 0.25 {
            let mut local = half;
            for k in half.saturating_sub(1)..=(half + 1).min(hi) {
                if self.mid_mag[k] > self.mid_mag[local] {
                    local = k;
                }
            }
            if self.mid_mag[local] >= median * F0_PROMINENCE {
                chosen = local;
            }
        }

        // Parabolic refinement on the log magnitude.
        let refined = if chosen > 0 && chosen + 1 <= hi {
            let a = self.mid_mag[chosen - 1].max(1e-12).ln();
            let b = self.mid_mag[chosen].max(1e-12).ln();
            let c = self.mid_mag[chosen + 1].max(1e-12).ln();
            let den = a - 2.0 * b + c;
            if den.abs() > 1e-9 {
                chosen as f64 + (0.5 * (a - c) / den) as f64
            } else {
                chosen as f64
            }
        } else {
            chosen as f64
        };

        // Continuity: while we were tracking, refuse an implausible jump.
        if self.f0_confident && self.f0_bin > 0.0 {
            let ratio = refined / self.f0_bin;
            if !(1.0 / F0_MAX_JUMP..=F0_MAX_JUMP).contains(&ratio) {
                // Allow it only if the new peak is clearly stronger; otherwise
                // hold, and let the next frame confirm.
                if best_v < median * F0_PROMINENCE * 2.0 {
                    return;
                }
            }
        }
        self.f0_bin = refined;
        self.f0_confident = true;
    }

    /// Assign each bin to the harmonic whose region it falls in.
    fn assign_harmonics(&mut self) {
        for k in 0..=self.top_bin {
            self.harmonic_of[k] = None;
        }
        if !self.f0_confident || self.f0_bin < 1.0 {
            return;
        }
        // Half the spacing between harmonics, which is where one region ends
        // and the next begins.
        let half = (self.f0_bin * 0.5).max(1.0);
        for h in 1..=MAX_HARMONICS {
            let centre = self.f0_bin * h as f64;
            if centre > self.top_bin as f64 {
                break;
            }
            let c = centre.round() as usize;
            let from = ((centre - half).floor().max(1.0)) as usize;
            let to = ((centre + half).ceil() as usize).min(self.top_bin);
            if from > to {
                continue;
            }
            // The anchor is the real local maximum, not the arithmetic multiple:
            // a slightly stretched string is not exactly harmonic.
            let mut a = c.clamp(from, to);
            for k in from..=to {
                if self.mid_mag[k] > self.mid_mag[a] {
                    a = k;
                }
            }
            self.anchor[h] = a;
            for k in from..=to {
                self.harmonic_of[k] = Some(h);
            }
        }
    }
}

/// One at `f <= lo`, zero at `f >= hi`, raised cosine between.
///
/// Amplitude-complementary with `1 - w`: the two bands add back to unity at
/// every frequency, so an unstretched render reconstructs exactly.
pub fn crossover_weight(f: f64, lo: f64, hi: f64) -> f32 {
    if f <= lo {
        1.0
    } else if f >= hi {
        0.0
    } else {
        let x = (f - lo) / (hi - lo);
        (0.5 + 0.5 * (std::f64::consts::PI * x).cos()) as f32
    }
}

/// Same rule the main engine uses: after EOF whatever is missing is the defined
/// zero padding rather than a reason to wait.
fn source_ready(ring: &InputRing, eof: bool, source_frames: u64, from: i64, to: i64) -> bool {
    let n = source_frames as i64;
    let need_to = to.min(n);
    if need_to <= from {
        return true;
    }
    if ring.holds(from.max(0), need_to) {
        return true;
    }
    eof
}

/// Analysis position for a synthesis time that may be negative during warm-up.
fn analysis_pos(map: &TimeMap, t: f64) -> f64 {
    if t >= 0.0 {
        map.inverse(t)
    } else {
        t * map.speed_at_output(0.0)
    }
}
