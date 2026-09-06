//! Phase vocoder with peak phase locking, transient reset and formant control.
//!
//! Per frame (dsp.md sec.5):
//!
//! ```text
//! omega_k       = 2*pi*k/N
//! delta_m[k]    = principal_arg(phi_m[k] - phi_(m-1)[k] - omega_k*Ha_m)
//! omega_hat[k]  = omega_k + delta_m[k]/Ha_m
//! theta_m[k]    = theta_(m-1)[k] + omega_hat[k]*Hs
//! Y_m[k]        = A_m[k] * exp(j*theta_m[k])
//! ```
//!
//! Three things this file is deliberate about:
//!
//! * `Ha_m > 0` always. A freeze is a separate state, never `Ha = 0` fed into
//!   the equation above.
//! * Channels are **not** decided independently. The instantaneous frequency is
//!   one energy-weighted estimate per bin and every channel is rotated by the
//!   same amount, so an identical pair stays identical, an inverted pair stays
//!   inverted, and a delayed pair keeps its delay. Peak partition and transient
//!   resets are shared for the same reason (system-design.md sec.9).
//! * Peak locking keeps the bins around a peak at their analysed phase offsets
//!   from that peak, so a partial does not come apart into separate bins.

use super::core::{EngineCore, Stepper};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::document::QualityProfile;
use crate::dsp::stft::{principal_arg, RealStft};
use crate::dsp::window::sqrt_hann_periodic;
use super::lowband::{crossover_weight, LowBand, LowBandConfig};
use crate::runtime::OutputAccum;
use realfft::num_complex::Complex32;

pub struct PvEngine {
    core: Option<EngineCore>,
    stft: Option<RealStft>,
    n: usize,
    hop: usize,
    bins: usize,
    /// Overlap-add warm-up rendered before output frame 0.
    lead: usize,
    window: Vec<f32>,
    // Per-channel state.
    prev_phase: Vec<Vec<f32>>,
    /// Analysed phase of the frame in flight, kept because `spec` is
    /// overwritten with the synthesis spectrum before the frame ends.
    cur_phase: Vec<Vec<f32>>,
    theta: Vec<Vec<f32>>,
    spec: Vec<Vec<Complex32>>,
    mag: Vec<Vec<f32>>,
    // Shared per-bin scratch.
    mag_sum: Vec<f32>,
    peak_of: Vec<usize>,
    omega_hat: Vec<f32>,
    env: Vec<f32>,
    env_log: Vec<f32>,
    env_cum: Vec<f32>,
    gain: Vec<f32>,
    time_in: Vec<f32>,
    time_out: Vec<f32>,
    // Transient detection, shared across channels.
    prev_mag_sum: Vec<f32>,
    /// Bins whose phase is reset this frame, decided once for all channels.
    reset_bin: Vec<bool>,
    /// Recent spectral flux, for an adaptive threshold.
    flux_ring: Vec<f32>,
    flux_pos: usize,
    flux_filled: usize,
    flux_scratch: Vec<f32>,
    /// Phase-lock strength; 1.0 is Laroche-Dolson identity locking.
    lock_beta: f32,
    /// Lowest bin a transient reset is allowed to touch.
    transient_guard_bin: usize,
    /// The main band's own overlap-add, merged with the low band's.
    acc: OutputAccum,
    /// `1 - w_low` on this band's grid; empty when there is no low band.
    hi_weight: Vec<f32>,
    /// The specialised path for everything below the crossover.
    low: Option<LowBand>,
    /// Crossover weight on the main grid, for the copy of the main band that
    /// carries the low frequencies itself.
    lo_weight: Vec<f32>,
    /// Output positions of recent transients, for the low-band gate.
    transient_out: Vec<u64>,
    /// Output position of the last re-anchor, for the refractory period.
    last_reset_out: Option<u64>,
    gate_pre: u64,
    gate_hold: u64,
    gate_release: u64,
    gate_enabled: bool,
    // Progress.
    raw_produced: u64,
    prev_source: f64,
    first_frame: bool,
    env_halfwidth: usize,
    /// `p/f`: the envelope warp that survives the later resampling stage.
    env_warp: f64,
    formant_active: bool,
    transients_hit: u64,
    // Hybrid (harmonic/percussive) state.
    hybrid: bool,
    /// Rolling magnitude history for the time-direction median.
    hist: Vec<Vec<f32>>,
    hist_pos: usize,
    hist_filled: usize,
    median_scratch: Vec<f32>,
    median_time: Vec<f32>,
    median_time_scratch: Vec<f32>,
    mask_h: Vec<f32>,
    harmonic_energy: f64,
    percussive_energy: f64,
}

/// Frames of history for the time-direction median, and bins for the
/// frequency-direction median. Both odd so the median is a real sample.
const HPSS_TIME: usize = 17;
const HPSS_FREQ: usize = 17;

/// Where the specialised low path hands over to the main engine.
///
/// The transition is a little under half an octave wide. Narrower rings in
/// time; wider drags the main engine down into frequencies its window cannot
/// resolve, which is the problem the split exists to avoid.
pub const CROSSOVER_LO_HZ: f64 = 150.0;
pub const CROSSOVER_HI_HZ: f64 = 210.0;

/// How long before and after a transient the low band steps aside, in seconds.
///
/// A 341 ms window cannot place an attack, so handing the bass to it across a
/// kick smears the drum's body backwards - measured on a drum fixture, the
/// attack rise went from 2.36 ms to 12.13 ms and pre-attack energy from
/// -75.8 dB to -2.8 dB. Around a transient the main engine takes the whole
/// spectrum back, which is what it was already good at.
/// Short edges, not a long fade. The low band and the main band's low-passed
/// copy are two different phase evolutions of the same signal, so any *slow*
/// crossfade between them is a time-varying comb filter: a 150 ms release
/// measured 90.98 cents of wobble on bass under drums against 2.47 with no gate
/// at all. Switching quickly, at the one moment the ear is busy with a
/// transient, is the only way a swap between them is affordable.
const LOW_GATE_PRE_S: f64 = 0.005;
const LOW_GATE_HOLD_S: f64 = 0.040;
const LOW_GATE_RELEASE_S: f64 = 0.008;
/// Transient positions kept for the gate. More than enough at any tempo.
const LOW_GATE_SLOTS: usize = 32;

/// A phase reset never touches bins below this frequency.
///
/// The point of resetting at a transient is to put the attack back where it
/// was inside the frame. That only means something where the window holds many
/// periods: at 60 Hz a 2048-point window at 48 kHz holds two and a half, so
/// resetting the fundamental sharpens nothing while breaking the continuity of
/// a note that is still sounding. A mastered track has bass under almost every
/// kick, so the cost lands on nearly every beat — measured on a 62 Hz note with
/// hits over it, the fundamental wandered 27.52 cents rms with the low bins
/// resetting and 0.60 with them held.
const TRANSIENT_GUARD_HZ: f64 = 160.0;

/// Below the guard, a bin resets only if it grew by at least this much.
///
/// A guard that simply never resets the low bins protects a sustained bass note
/// but also refuses to reset a kick drum, whose energy *is* low: the same
/// fixture set measured the drum attack going from 2.37 ms to 4.75 ms and its
/// pre-echo from -84.8 dB to -27.4 dB. Growth is what separates the two cases.
/// A bin that was already loud is a note still sounding and gets held; a bin
/// that jumped is the attack itself and gets reset.
const LOW_BIN_RISE_DB: f32 = 6.0;

/// Frames of spectral-flux history behind the adaptive transient threshold.
const FLUX_HISTORY: usize = 43;
/// A frame is a transient when its flux exceeds this multiple of the local
/// median *and* the share of the frame below. The median test alone fires on
/// steady material, where the median is near zero and any ripple beats it.
const FLUX_TRIGGER: f32 = 3.0;
/// ...and when the growth is at least this share of the frame's magnitude. A
/// steady tone sits near 0.01 here; a drum hit is several tenths.
const FLUX_SHARE: f32 = 0.22;


impl Default for PvEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PvEngine {
    pub fn new() -> Self {
        Self {
            core: None,
            stft: None,
            n: 0,
            hop: 0,
            bins: 0,
            lead: 0,
            window: Vec::new(),
            prev_phase: Vec::new(),
            cur_phase: Vec::new(),
            theta: Vec::new(),
            spec: Vec::new(),
            mag: Vec::new(),
            mag_sum: Vec::new(),
            peak_of: Vec::new(),
            omega_hat: Vec::new(),
            env: Vec::new(),
            env_log: Vec::new(),
            env_cum: Vec::new(),
            gain: Vec::new(),
            time_in: Vec::new(),
            time_out: Vec::new(),
            prev_mag_sum: Vec::new(),
            reset_bin: Vec::new(),
            flux_ring: Vec::new(),
            flux_pos: 0,
            flux_filled: 0,
            flux_scratch: Vec::new(),
            lock_beta: 1.0,
            transient_guard_bin: 0,
            acc: OutputAccum::new(1, 1),
            hi_weight: Vec::new(),
            low: None,
            lo_weight: Vec::new(),
            transient_out: Vec::new(),
            last_reset_out: None,
            gate_pre: 0,
            gate_hold: 0,
            gate_release: 0,
            gate_enabled: true,
            raw_produced: 0,
            prev_source: 0.0,
            first_frame: true,
            env_halfwidth: 0,
            env_warp: 1.0,
            formant_active: false,
            transients_hit: 0,
            hybrid: false,
            hist: Vec::new(),
            hist_pos: 0,
            hist_filled: 0,
            median_scratch: Vec::new(),
            median_time: Vec::new(),
            median_time_scratch: Vec::new(),
            mask_h: Vec::new(),
            harmonic_energy: 0.0,
            percussive_energy: 0.0,
        }
    }

    /// Same engine, harmonic/percussive split enabled.
    pub fn hybrid() -> Self {
        let mut e = Self::new();
        e.hybrid = true;
        e
    }

    /// Energy that ended up in each branch on the last frame. A leakage
    /// diagnostic for the UI, not a quality score.
    pub fn branch_energy(&self) -> (f64, f64) {
        (self.harmonic_energy, self.percussive_energy)
    }

    pub const CAPABILITY: Capability = Capability {
        name: "polyphonic",
        min_ratio: 0.1,
        max_ratio: 10.0,
        independent_pitch: false,
        formant_control: true,
        realtime_safe: true,
    };

    pub fn fft_size(&self) -> usize {
        self.n
    }
    pub fn transients_hit(&self) -> u64 {
        self.transients_hit
    }

    /// The low band's window and how often it held a fundamental, for the UI
    /// and for the quality battery. `None` when there is no low band.
    pub fn low_band_report(&self) -> Option<(usize, f64, Option<f64>)> {
        self.low
            .as_ref()
            .map(|l| (l.window_size(), l.tracking_ratio(), l.tracked_hz()))
    }
}

/// Analysis position for a synthesis time that may be negative during warm-up.
/// Before zero the read extrapolates backwards at the initial speed, which
/// lands in the defined zero padding rather than repeating frame 0.
fn analysis_pos(map: &crate::mapping::TimeMap, t: f64) -> f64 {
    if t >= 0.0 {
        map.inverse(t)
    } else {
        t * map.speed_at_output(0.0)
    }
}

/// Smooth a magnitude spectrum in the log domain into a spectral envelope.
///
/// A moving average over a fixed bin width: a deliberately crude source-filter
/// approximation, not a vocal-tract model (dsp.md sec.8). `log` and `cum` are
/// caller-owned scratch so nothing allocates here.
fn spectral_envelope(mag: &[f32], half: usize, log: &mut [f32], cum: &mut [f32], out: &mut [f32]) {
    let n = mag.len();
    if half == 0 {
        out[..n].copy_from_slice(mag);
        return;
    }
    for i in 0..n {
        log[i] = mag[i].max(1e-9).ln();
    }
    cum[0] = 0.0;
    for i in 0..n {
        cum[i + 1] = cum[i] + log[i];
    }
    for i in 0..n {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        let mean = (cum[hi] - cum[lo]) / (hi - lo) as f32;
        out[i] = mean.exp();
    }
}

/// Median of a slice, using a caller-owned scratch buffer.
fn median_into(values: &[f32], scratch: &mut [f32]) -> f32 {
    let n = values.len();
    scratch[..n].copy_from_slice(values);
    let s = &mut scratch[..n];
    s.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[n / 2]
}

/// How much of the bass the low band is trusted with at an output frame.
///
/// One at rest, dipping to zero across a transient and recovering over the
/// release. The main band's low-passed copy takes over exactly as much as the
/// low band gives up, so the two always sum to the whole spectrum. Free
/// function so it does not borrow the engine while `core` is borrowed mutably.
fn low_gate(transients: &[u64], o: u64, pre: u64, hold: u64, release: u64) -> f32 {
    let mut g = 1.0f32;
    for &tp in transients {
        let d = o as i64 - tp as i64;
        if d < -(pre as i64) || d > (release + hold) as i64 {
            continue;
        }
        let v = if d < 0 {
            // Closing edge.
            let x = (-d) as f32 / pre.max(1) as f32;
            0.5 - 0.5 * (std::f32::consts::PI * (1.0 - x)).cos()
        } else if (d as u64) < hold {
            0.0
        } else {
            // Opening edge.
            let x = (d as u64 - hold) as f32 / release.max(1) as f32;
            0.5 - 0.5 * (std::f32::consts::PI * x).cos()
        };
        g = g.min(v);
    }
    g
}

impl Stepper for PvEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    /// Advance whichever band is behind, then hand over what both have.
    ///
    /// The two bands run on their own schedules - the low one has an eight
    /// times longer window and hop - so neither drives the other. They meet at
    /// the merge, which can only deliver output frames both have finished.
    fn step(&mut self) -> bool {
        let mut progress = false;
        {
            let core = self.core.as_mut().expect("prepared");
            core.out.compact();
        }
        self.acc.compact();
        if let Some(l) = self.low.as_mut() {
            l.acc.compact();
        }

        // Whichever band is short of deliverable frames gets a turn.
        let lo_ready = self.low.as_ref().map(|l| l.ready()).unwrap_or(usize::MAX);
        if self.acc.ready() <= lo_ready {
            progress |= self.step_main();
        }
        if self.low.is_some() && lo_ready <= self.acc.ready() {
            progress |= self.step_low();
        }
        progress |= self.merge();

        // Keep source back to whichever band is reading furthest behind.
        let core = self.core.as_mut().expect("prepared");
        let next_hi = analysis_pos(
            &core.cfg.map,
            self.raw_produced as f64 - self.lead as f64,
        );
        let next_lo = self
            .low
            .as_ref()
            .map(|l| l.next_source(&core.cfg.map))
            .unwrap_or(next_hi);
        core.ring
            .discard_before(next_hi.min(next_lo).floor().max(0.0) as u64);
        progress
    }
}

impl PvEngine {
    fn low_present_after(&self, low: &Option<LowBand>) -> bool {
        low.is_some()
    }

    fn step_low(&mut self) -> bool {
        let Some(low) = self.low.as_mut() else {
            return false;
        };
        let core = self.core.as_ref().expect("prepared");
        low.step(
            &core.ring,
            &core.cfg.map,
            core.eof,
            core.cfg.source_frames(),
            core.total_output(),
        )
    }

    /// Sum the two bands into the engine's output. Only frames both bands have
    /// finished can go: an amplitude-complementary split is only complementary
    /// when both halves are present.
    fn merge(&mut self) -> bool {
        let core = self.core.as_mut().expect("prepared");
        let lo_ready = self.low.as_ref().map(|l| l.ready()).unwrap_or(usize::MAX);
        let n = self
            .acc
            .ready()
            .min(lo_ready)
            .min(core.out.space())
            .min(core.remaining_output() as usize);
        if n == 0 {
            return false;
        }
        let ch = core.cfg.channels;
        match self.low.as_ref() {
            None => {
                for c in 0..ch {
                    for i in 0..n {
                        core.out.add(c, i, self.acc.ready_at(c, i));
                    }
                }
            }
            Some(l) => {
                // out = main_full + g * (low_band - main_lowpassed)
                //
                // At g = 0 this is exactly the single-band engine; at g = 1 it
                // is the split. In between it is still the whole spectrum,
                // because the two low-frequency versions swap places rather
                // than being mixed in.
                let base = core.synth_pos;
                for i in 0..n {
                    let g = if !self.gate_enabled {
                        1.0
                    } else {
                        low_gate(
                        &self.transient_out,
                        base + i as u64,
                        self.gate_pre,
                        self.gate_hold,
                        self.gate_release,
                        )
                    };
                    for c in 0..ch {
                        let full = self.acc.ready_at(c, i);
                        let main_low = self.acc.ready_at(ch + c, i);
                        let band_low = l.acc.ready_at(c, i);
                        core.out.add(c, i, full + g * (band_low - main_low));
                    }
                }
            }
        }
        core.out.advance(n);
        let horizon = core.synth_pos + n as u64;
        let rel = self.gate_release;
        self.transient_out.retain(|t| t + rel >= horizon.saturating_sub(rel));
        self.acc.consume(n);
        if let Some(l) = self.low.as_mut() {
            l.acc.consume(n);
        }
        core.synth_pos += n as u64;
        true
    }

    fn step_main(&mut self) -> bool {
        let core = self.core.as_mut().expect("prepared");
        let stft = self.stft.as_mut().expect("prepared");
        let n = self.n;
        let hop = self.hop;
        let bins = self.bins;
        let channels = core.cfg.channels;

        let raw_target = core.total_output() + self.lead as u64;
        if self.raw_produced >= raw_target {
            return false;
        }
        if self.acc.space() < n {
            return false;
        }

        let t = self.raw_produced as f64 - self.lead as f64;
        let s = analysis_pos(&core.cfg.map, t);
        let s0 = s.floor() as i64;
        // The map puts the analysis window at a fractional position. Reading at
        // `floor(s)` and then measuring the phase advance against the exact
        // `s - s_prev` mixes two different hops, and the mismatch is a
        // frequency-proportional phase error every frame - which is exactly
        // what a metallic edge sounds like. The fraction is applied as a linear
        // phase ramp on the analysed spectrum below, which is an exact
        // fractional shift for a windowed frame and costs one rotation a bin.
        let frac = (s - s0 as f64) as f32;
        if !core.source_ready(s0, s0 + n as i64) {
            return false;
        }

        // --- analysis -------------------------------------------------------
        let mut ha = (s - self.prev_source).max(1e-3);
        if self.first_frame {
            ha = hop as f64 * core.cfg.map.speed_at_output(0.0).max(1e-6);
        }

        for c in 0..channels {
            for i in 0..n {
                self.time_in[i] = core.ring.at(c, s0 + i as i64) * self.window[i];
            }
            stft.forward(&self.time_in, &mut self.spec[c]);
            if frac != 0.0 {
                // z[n] = y[n + frac]  =>  Z[k] = Y[k] * exp(+j*omega_k*frac)
                for k in 0..bins {
                    let a = std::f32::consts::TAU * k as f32 / n as f32 * frac;
                    let (sa, ca) = a.sin_cos();
                    let v = self.spec[c][k];
                    self.spec[c][k] = Complex32::new(v.re * ca - v.im * sa, v.re * sa + v.im * ca);
                }
            }
            for k in 0..bins {
                self.mag[c][k] = self.spec[c][k].norm();
                self.cur_phase[c][k] = self.spec[c][k].arg();
            }
        }
        for k in 0..bins {
            let mut sum = 0.0;
            for c in 0..channels {
                sum += self.mag[c][k];
            }
            self.mag_sum[k] = sum;
        }

        // --- one shared instantaneous frequency per bin ---------------------
        let two_pi = std::f32::consts::TAU;
        for k in 0..bins {
            let omega_k = two_pi * k as f32 / n as f32;
            let expected = omega_k * ha as f32;
            // Energy-weighted circular mean of the per-channel phase advances.
            let mut vx = 0.0f32;
            let mut vy = 0.0f32;
            for c in 0..channels {
                let phi = self.cur_phase[c][k];
                let d = principal_arg(phi - self.prev_phase[c][k] - expected);
                let w = self.mag[c][k];
                vx += w * d.cos();
                vy += w * d.sin();
            }
            let d = if vx == 0.0 && vy == 0.0 { 0.0 } else { vy.atan2(vx) };
            self.omega_hat[k] = omega_k + d / ha as f32;
        }

        // --- shared peak partition -----------------------------------------
        // A bin is a peak when it dominates its two neighbours on both sides;
        // every other bin follows its nearest peak.
        let mut last_peak = 0usize;
        let mut peaks: usize = 0;
        for k in 0..bins {
            let m = self.mag_sum[k];
            let is_peak = k >= 2
                && k + 2 < bins
                && m > self.mag_sum[k - 1]
                && m > self.mag_sum[k - 2]
                && m > self.mag_sum[k + 1]
                && m > self.mag_sum[k + 2];
            if is_peak {
                // Bins between the previous peak and this one split at the
                // valley midpoint.
                let mid = (last_peak + k) / 2;
                for j in last_peak..=mid.min(k) {
                    self.peak_of[j] = last_peak;
                }
                for j in mid + 1..=k {
                    self.peak_of[j] = k;
                }
                last_peak = k;
                peaks += 1;
            }
        }
        for j in last_peak..bins {
            self.peak_of[j] = last_peak;
        }
        if peaks == 0 {
            for j in 0..bins {
                self.peak_of[j] = j;
            }
        }

        // --- transient decision, shared across channels ---------------------
        //
        // Two sources agree on one answer. The plan's protections come from
        // offline onset analysis and are exact but only exist when there was an
        // analysis; the spectral flux below works from the frame in hand, so a
        // live stream with no analysis still keeps its attacks.
        let frame_from = s0;
        let frame_to = s0 + n as i64;
        let planned = super::protect_starts_in(&core.cfg.protections, frame_from, frame_to);

        let mut flux = 0.0f32;
        let mut total = 0.0f32;
        for k in 0..bins {
            let d = self.mag_sum[k] - self.prev_mag_sum[k];
            if d > 0.0 {
                flux += d;
            }
            total += self.mag_sum[k];
        }
        let share = flux / total.max(1e-9);
        let detected = if core.cfg.transient_protect && self.flux_filled >= 8 {
            let hn = self.flux_filled;
            let med = median_into(&self.flux_ring[..hn], &mut self.flux_scratch);
            flux > med * FLUX_TRIGGER && share > FLUX_SHARE
        } else {
            false
        };
        self.flux_ring[self.flux_pos] = flux;
        self.flux_pos = (self.flux_pos + 1) % FLUX_HISTORY;
        self.flux_filled = (self.flux_filled + 1).min(FLUX_HISTORY);

        // Attack Protect's length is a refractory period here: the frames
        // just after a re-anchor still belong to the attack that caused it.
        let at_out = self.raw_produced.saturating_sub(self.lead as u64);
        let in_refractory = match self.last_reset_out {
            Some(l) => at_out < l + core.cfg.protect_frames,
            None => false,
        };
        let reset = (planned || detected) && !in_refractory;
        if reset {
            self.last_reset_out = Some(at_out);
            self.transients_hit += 1;
            // Where this frame lands on the output timeline, for the gate.
            if self.transient_out.len() >= LOW_GATE_SLOTS {
                self.transient_out.remove(0);
            }
            self.transient_out.push(at_out);
        }

        // A blanket reset restarts the reverb tail and the held chord along
        // with the hit. Reset only the bins that actually grew; the rest keep
        // propagating (dsp.md sec.5, "protect the attack then spread the
        // stretch over the sustain").
        // Every bin, not just the ones that grew.
        //
        // A partial reset leaves the unrisen bins on their propagated phase, so
        // their contribution lands at the wrong time inside the frame and
        // smears the attack it was meant to protect: measured on a drum fixture
        // at +3 semitones, pre-attack energy was -41.5 dB with a partial reset
        // against -102.9 dB with a full one. Setting every bin back to the
        // analysed phase makes the frame reconstruct the windowed input
        // exactly, which is the whole point of resetting at a transient. The
        // price is a one-frame phase discontinuity in whatever was sustaining,
        // which is why the detector below has to be strict.
        let rise = 10f32.powf(LOW_BIN_RISE_DB / 20.0);
        for k in 0..bins {
            self.reset_bin[k] = reset
                && (k >= self.transient_guard_bin
                    || self.mag_sum[k] > self.prev_mag_sum[k].max(1e-9) * rise);
        }
        self.prev_mag_sum[..bins].copy_from_slice(&self.mag_sum[..bins]);

        // --- harmonic / percussive masks ------------------------------------
        //
        // Both branches come out of the *same* STFT frame and are summed in the
        // same spectral frame, so branch alignment is exact by construction and
        // there is no per-branch delay to compensate. Mask leakage is still a
        // real risk and is reported through `branch_energy`.
        if self.hybrid {
            self.hist[self.hist_pos][..bins].copy_from_slice(&self.mag_sum[..bins]);
            self.hist_pos = (self.hist_pos + 1) % HPSS_TIME;
            self.hist_filled = (self.hist_filled + 1).min(HPSS_TIME);

            let half_f = HPSS_FREQ / 2;
            let mut he = 0.0f64;
            let mut pe = 0.0f64;
            for k in 0..bins {
                // Harmonic: median along time at this bin.
                let hn = self.hist_filled.max(1);
                for j in 0..hn {
                    self.median_time[j] = self.hist[j][k];
                }
                let h = median_into(&self.median_time[..hn], &mut self.median_time_scratch);
                // Percussive: median across frequency in this frame.
                let lo = k.saturating_sub(half_f);
                let hi = (k + half_f + 1).min(bins);
                let p = median_into(&self.mag_sum[lo..hi], &mut self.median_scratch);
                let (h2, p2) = ((h * h) as f64, (p * p) as f64);
                let m = if h2 + p2 > 1e-20 { h2 / (h2 + p2) } else { 0.5 };
                self.mask_h[k] = m as f32;
                he += h2 * m;
                pe += p2 * (1.0 - m);
            }
            self.harmonic_energy = he;
            self.percussive_energy = pe;
        }

        // --- phase propagation ---------------------------------------------
        //
        // Every channel is rotated by the same `omega_hat` and shares the same
        // peak partition and the same reset set, so an identical pair stays
        // identical, an inverted pair stays inverted, and a delayed pair keeps
        // its delay. Nothing here is decided per channel.
        for c in 0..channels {
            if self.first_frame {
                for k in 0..bins {
                    self.theta[c][k] = self.cur_phase[c][k];
                }
                continue;
            }
            // Peaks accumulate; followers hold their analysed offset from
            // the peak they belong to. Transient bins are dealt with after
            // this, once every channel has been advanced.
            for k in 0..bins {
                if self.peak_of[k] == k {
                    self.theta[c][k] += self.omega_hat[k] * hop as f32;
                }
            }
            for k in 0..bins {
                let pk = self.peak_of[k];
                if pk != k {
                    let offset = principal_arg(self.cur_phase[c][k] - self.cur_phase[c][pk]);
                    self.theta[c][k] = self.theta[c][pk] + self.lock_beta * offset;
                }
            }
        }

        // --- transient re-anchor ---------------------------------------------
        //
        // Setting the phase of a transient frame straight back to the analysed
        // values is what sharpens the attack, and it is also what made Attack
        // Protect sound like a gate. Four frames overlap at this hop, so a
        // frame whose phases jump no longer agrees with the three already
        // written around it and the overlap-add cancels: measured on a drum
        // fixture, turning protection on cost 1.73 dB of mean level and dug a
        // 61 dB hole at the hits. Nothing multiplied anything - the level fell
        // out of the interference.
        //
        // So keep the analysed phase *structure*, which is the part that
        // sharpens the attack, and rotate the whole set by the one angle that
        // best matches the trajectory it is replacing. That angle is the
        // magnitude-weighted circular mean of the disagreement, it is shared by
        // every channel and every bin, and a common rotation moves no partial
        // relative to another. No crossfade between trajectories, and no gain
        // anywhere.
        if !self.first_frame {
            // The disagreement between the trajectory and the analysed phase is
            // mostly a *delay*: propagation advances each bin by its own
            // frequency over the synthesis hop while the analysis moved by the
            // shorter analysis hop, and that difference is linear in frequency.
            // So fit a delay and a constant rather than a constant alone. A
            // delay is a pure time shift, which leaves the waveform - and the
            // attack - exactly as sharp as the analysis found it, where a bare
            // constant rotation is a Hilbert-like twist that measurably
            // softened the attack (rise 2.36 ms to 3.56 ms).
            //
            // The slope is estimated from the phase difference between adjacent
            // bins, which is immune to wrapping: for a delay tau the difference
            // is a constant (2*pi/N)*tau whatever the absolute phases are.
            let mut any = false;
            let mut sx = 0.0f32;
            let mut sy = 0.0f32;
            let mut prev: Option<(f32, f32)> = None;
            for k in 0..bins {
                if !self.reset_bin[k] {
                    prev = None;
                    continue;
                }
                any = true;
                let mut dx = 0.0f32;
                let mut dy = 0.0f32;
                for c in 0..channels {
                    let w = self.mag[c][k];
                    let d = self.theta[c][k] - self.cur_phase[c][k];
                    dx += w * d.cos();
                    dy += w * d.sin();
                }
                if let Some((px, py)) = prev {
                    // Complex product with the conjugate of the previous bin:
                    // its argument is the bin-to-bin phase step.
                    sx += dx * px + dy * py;
                    sy += dy * px - dx * py;
                }
                prev = Some((dx, dy));
            }
            if any {
                let step = if sx == 0.0 && sy == 0.0 { 0.0 } else { sy.atan2(sx) };
                let tau = step * n as f32 / std::f32::consts::TAU;
                // A wild slope estimate would move the attack; keep it to a
                // fraction of the window.
                let tau = tau.clamp(-(n as f32) / 8.0, n as f32 / 8.0);
                // Residual constant, after the delay is taken out.
                let mut vx = 0.0f32;
                let mut vy = 0.0f32;
                for k in 0..bins {
                    if !self.reset_bin[k] {
                        continue;
                    }
                    let ramp = std::f32::consts::TAU * k as f32 / n as f32 * tau;
                    for c in 0..channels {
                        let w = self.mag[c][k];
                        let d = self.theta[c][k] - self.cur_phase[c][k] - ramp;
                        vx += w * d.cos();
                        vy += w * d.sin();
                    }
                }
                let phi0 = if vx == 0.0 && vy == 0.0 { 0.0 } else { vy.atan2(vx) };
                for k in 0..bins {
                    if !self.reset_bin[k] {
                        continue;
                    }
                    let ramp = std::f32::consts::TAU * k as f32 / n as f32 * tau + phi0;
                    for c in 0..channels {
                        self.theta[c][k] = self.cur_phase[c][k] + ramp;
                    }
                }
            }
        }

        // --- formant correction ---------------------------------------------
        if self.formant_active {
            spectral_envelope(
                &self.mag_sum,
                self.env_halfwidth,
                &mut self.env_log,
                &mut self.env_cum,
                &mut self.env,
            );
            let warp = self.env_warp;
            for k in 0..bins {
                let src = k as f64 * warp;
                let target = if src <= 0.0 {
                    self.env[0]
                } else if src >= (bins - 1) as f64 {
                    // Above the top bin the envelope is unknown; hold the last
                    // value rather than inventing a rolloff.
                    self.env[bins - 1]
                } else {
                    let i = src.floor() as usize;
                    let f = (src - i as f64) as f32;
                    self.env[i] * (1.0 - f) + self.env[i + 1] * f
                };
                let g = target / self.env[k].max(1e-9);
                // Clamp: an envelope ratio is an approximation, and a silent
                // bin must never be boosted into noise.
                self.gain[k] = g.clamp(0.25, 4.0);
            }
        }

        // --- synthesis -------------------------------------------------------
        for c in 0..channels {
            for k in 0..bins {
                let a = if self.formant_active {
                    self.mag[c][k] * self.gain[k]
                } else {
                    self.mag[c][k]
                };
                let th = self.theta[c][k];
                if self.hybrid {
                    // Harmonic bins ride the propagated phase; percussive bins
                    // keep the analysed phase, which is what holds an attack in
                    // place instead of smearing it across the frame.
                    let mh = self.mask_h[k];
                    let ph = self.cur_phase[c][k];
                    let re = a * (mh * th.cos() + (1.0 - mh) * ph.cos());
                    let im = a * (mh * th.sin() + (1.0 - mh) * ph.sin());
                    self.spec[c][k] = Complex32::new(re, im);
                } else {
                    self.spec[c][k] = Complex32::new(a * th.cos(), a * th.sin());
                }
            }
            // A real signal has no imaginary part at DC or Nyquist.
            self.spec[c][0].im = 0.0;
            self.spec[c][bins - 1].im = 0.0;
            stft.inverse(&self.spec[c], &mut self.time_out);
            for i in 0..n {
                self.acc.add(c, i, self.time_out[i] * self.window[i]);
            }
            // The same frame again, low-passed. Scaling the finished spectrum
            // rather than rebuilding it matters: in Hybrid the full spectrum
            // has already been through the harmonic/percussive blend, and a
            // copy built from `theta` alone would not be the low-passed version
            // of what was just synthesised, so `full - main_low` would remove
            // the wrong signal.
            if !self.lo_weight.is_empty() {
                for k in 0..bins {
                    let w = self.lo_weight[k];
                    self.spec[c][k] = Complex32::new(
                        self.spec[c][k].re * w,
                        self.spec[c][k].im * w,
                    );
                }
                self.spec[c][0].im = 0.0;
                self.spec[c][bins - 1].im = 0.0;
                stft.inverse(&self.spec[c], &mut self.time_out);
                for i in 0..n {
                    self.acc.add(channels + c, i, self.time_out[i] * self.window[i]);
                }
            }
        }
        for i in 0..n {
            self.acc.add_norm(i, self.window[i] * self.window[i]);
        }

        for c in 0..channels {
            self.prev_phase[c][..bins].copy_from_slice(&self.cur_phase[c][..bins]);
        }

        let advance = hop.min((raw_target - self.raw_produced) as usize);
        self.acc.advance_normalized(advance, 1e-4);
        self.raw_produced += advance as u64;
        self.prev_source = s;
        self.first_frame = false;
        true
    }
}

impl StretchEngine for PvEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        let (lo, hi) = cfg.map.ratio_range();
        let cap = Self::CAPABILITY;
        if lo < cap.min_ratio || hi > cap.max_ratio {
            return Err(PrepareError::UnsupportedRatio {
                requested: if lo < cap.min_ratio { lo } else { hi },
                min: cap.min_ratio,
                max: cap.max_ratio,
            });
        }

        let n = match cfg.stft_size {
            Some(n) => n.next_power_of_two().clamp(256, 16384),
            None => {
                let base = match cfg.quality {
                    QualityProfile::Offline => 2048,
                    QualityProfile::Realtime => 1024,
                };
                // The separation needs a longer window than the synthesis does.
                //
                // HPSS estimates the percussive part as a median across
                // frequency, which only finds the broadband floor when the
                // window resolves the partials either side of it. A bass note
                // puts its partials f0 apart, so a 2048-point window at 48 kHz
                // leaves a 55 Hz comb only 2.35 bins apart, the median lands on
                // the partials themselves, and the mask collapses toward 0.5 —
                // half the bass then gets the percussive path's analysed phase
                // and warbles. Measured on a 55 Hz harmonic tone at +3
                // semitones, the fundamental wandered 10.48 cents rms at 2048,
                // 1.14 at 4096 and 0.43 at 8192, while plain polyphonic barely
                // moved (0.37 / 0.30 / 0.21) — so this is the mask, not the
                // phase propagation.
                //
                // 4096 is where that stops being audible without paying the
                // 170 ms of smearing an 8192 window costs a mix with drums in
                // it. Plain polyphonic keeps the shorter window: the same
                // measurement shows it slightly *worse* at 4096 above 110 Hz.
                let base = if self.hybrid { base * 2 } else { base };
                if cfg.sample_rate > 60_000 { base * 2 } else { base }
            }
        };
        let hop = n / 4;
        let bins = n / 2 + 1;
        let ch = cfg.channels;

        self.stft = Some(RealStft::new(n));
        self.n = n;
        self.hop = hop;
        self.bins = bins;
        self.lead = n - hop;
        self.window = sqrt_hann_periodic(n);
        self.prev_phase = vec![vec![0.0; bins]; ch];
        self.cur_phase = vec![vec![0.0; bins]; ch];
        self.theta = vec![vec![0.0; bins]; ch];
        self.spec = vec![vec![Complex32::new(0.0, 0.0); bins]; ch];
        self.mag = vec![vec![0.0; bins]; ch];
        self.mag_sum = vec![0.0; bins];
        self.prev_mag_sum = vec![0.0; bins];
        self.reset_bin = vec![false; bins];
        self.flux_ring = vec![0.0; FLUX_HISTORY];
        self.flux_pos = 0;
        self.flux_filled = 0;
        self.flux_scratch = vec![0.0; FLUX_HISTORY];
        self.peak_of = (0..bins).collect();
        self.omega_hat = vec![0.0; bins];
        self.env = vec![0.0; bins];
        self.env_log = vec![0.0; bins];
        self.env_cum = vec![0.0; bins + 1];
        self.gain = vec![1.0; bins];
        self.time_in = vec![0.0; n];
        self.time_out = vec![0.0; n];
        self.raw_produced = 0;
        self.prev_source = 0.0;
        self.first_frame = true;
        self.transients_hit = 0;
        self.hist = vec![vec![0.0; bins]; HPSS_TIME];
        self.hist_pos = 0;
        self.hist_filled = 0;
        self.median_scratch = vec![0.0; bins.max(HPSS_TIME)];
        self.median_time = vec![0.0; HPSS_TIME];
        self.median_time_scratch = vec![0.0; HPSS_TIME];
        self.mask_h = vec![1.0; bins];

        // Smooth across roughly 250 Hz: wide enough to average out the harmonic
        // comb of a low voice, narrow enough to keep formant peaks apart.
        // Identity phase locking: a follower holds exactly the offset from its
        // peak that the analysis measured. Scaling that offset was tried and
        // measured badly - a vowel wobbled 13.09 cents rms against 0.19 for
        // identity - because the offset describes the *shape* of a partial
        // across its bins, and shrinking it deforms the partial rather than
        // relaxing anything. Kept as a constant so the intent is explicit.
        self.lock_beta = 1.0;
        self.transient_guard_bin =
            (TRANSIENT_GUARD_HZ * n as f64 / cfg.sample_rate as f64).ceil() as usize;

        let hz_per_bin = cfg.sample_rate as f64 / n as f64;
        self.env_halfwidth = ((250.0 / hz_per_bin) * 0.5).round().max(1.0) as usize;
        let p = cfg.pitch;
        let f = cfg.formant.factor(p);
        self.env_warp = p / f.max(1e-6);
        self.formant_active = (self.env_warp - 1.0).abs() > 1e-6;

        // --- the specialised low path ---------------------------------------
        //
        // Only where it can pay for itself: it needs a window long enough to
        // resolve a bass fundamental, which is 8x the main one, and that is
        // only affordable and only sensible offline or on a realtime profile
        // that already accepts lookahead. Bypass/Tape never get here.
        let low_n = match cfg.low_stft_size {
            Some(0) => 0,
            Some(v) => v.next_power_of_two(),
            None => {
                let base = match cfg.quality {
                    QualityProfile::Offline => 8192,
                    QualityProfile::Realtime => 4096,
                };
                if cfg.sample_rate > 60_000 { base * 2 } else { base }
            }
        };
        let low = if low_n > n {
            Some(LowBand::new(&LowBandConfig {
                sample_rate: cfg.sample_rate,
                channels: ch,
                n: low_n,
                crossover_lo_hz: CROSSOVER_LO_HZ,
                crossover_hi_hz: CROSSOVER_HI_HZ,
                max_block: cfg.max_block,
            }))
        } else {
            // The main window is already as long as the low one would be, so a
            // split would buy nothing and cost a crossover.
            None
        };

        if low.is_some() {
            let hz_per_bin = cfg.sample_rate as f64 / n as f64;
            self.lo_weight = (0..bins)
                .map(|k| {
                    crossover_weight(
                        k as f64 * hz_per_bin,
                        CROSSOVER_LO_HZ,
                        CROSSOVER_HI_HZ,
                    )
                })
                .collect();
            self.hi_weight = self.lo_weight.iter().map(|w| 1.0 - w).collect();
        } else {
            self.lo_weight = Vec::new();
            self.hi_weight = Vec::new();
        }
        self.transient_out.clear();
        self.gate_pre = (cfg.sample_rate as f64 * LOW_GATE_PRE_S).round() as u64;
        self.gate_hold = (cfg.sample_rate as f64 * LOW_GATE_HOLD_S).round() as u64;
        self.gate_release = if cfg.low_gate {
            (cfg.sample_rate as f64 * LOW_GATE_RELEASE_S).round() as u64
        } else {
            0
        };
        self.gate_enabled = cfg.low_gate;

        let widest = low.as_ref().map(|l| l.window_size()).unwrap_or(n).max(n);
        let max_speed = 1.0 / lo.max(1e-6);
        let span = (cfg.max_block as f64 * max_speed).ceil() as usize;
        // The two bands read at different positions, so the ring has to span
        // the gap between them plus the wider window.
        let ring = span + 3 * widest + cfg.max_block + 64;
        let out_cap = (cfg.max_block + 4 * n).max(8192);

        // The main band synthesises twice: the full spectrum, and its own
        // low-passed copy, carried as extra channels in one accumulator so a
        // single window-sum plane normalises both. The merge blends between
        // that copy and the low band, so the crossover can be moved per output
        // sample without ever ceasing to sum to unity.
        let acc_ch = if self.low_present_after(&low) { ch * 2 } else { ch };
        self.acc = OutputAccum::with_norm(acc_ch, out_cap);
        self.acc.skip(self.lead);
        self.low = low;

        let merged_cap = (cfg.max_block * 2 + 4 * n).max(8192);
        let mut core = EngineCore::new(cfg.clone(), ring, merged_cap);
        core.out = OutputAccum::new(ch, merged_cap);
        self.core = Some(core);
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        let lead = self.lead;
        let (n, bins, ch) = (self.n, self.bins, self.prev_phase.len());
        // The low band starts its warm-up a whole window earlier than the main
        // one, so a seek has to place the input ring where *it* begins reading,
        // not where the main band does. Sizing this from the main band left the
        // low band permanently short of source: it produced nothing, the merge
        // produced nothing, and a swapped-in voice arrived silent - which is
        // what stopped the crossfade from ever completing and made every later
        // plan change get dropped.
        let widest_lead = self
            .low
            .as_ref()
            .map(|l| l.lead().max(lead))
            .unwrap_or(lead);
        let widest_n = self
            .low
            .as_ref()
            .map(|l| l.window_size().max(n))
            .unwrap_or(n);
        if let Some(core) = self.core.as_mut() {
            let t = position.output_frame as f64 - widest_lead as f64;
            let s = analysis_pos(&core.cfg.map, t);
            let start = (s.floor() as i64 - widest_n as i64).max(0) as u64;
            core.reset_to(start);
            // No skip here: the warm-up belongs to the band accumulators, and
            // `core.out` now only ever holds merged output. Skipping it again
            // threw away the first `lead` real frames, so a seeked render
            // stopped exactly `lead` short of `M`.
            core.synth_pos = position.output_frame;
            core.delivered = position.output_frame;
        }
        // `raw_produced` counts raw frames including the warm-up, and
        // `out.skip(lead)` discards that warm-up. Adding `lead` here as well
        // would double-count it: the first `lead` real output frames would be
        // thrown away and the engine would stop `lead` frames short of `M`,
        // so it would never report Finished. Offline never hits this because
        // it never seeks.
        self.raw_produced = position.output_frame;
        self.acc.reset();
        self.acc.skip(lead);
        self.transient_out.clear();
        self.last_reset_out = None;
        // The detector carries history: without clearing it a seek would make
        // different transient decisions than a fresh prepare, and the render
        // would not repeat.
        self.prev_mag_sum.fill(0.0);
        self.flux_ring.fill(0.0);
        self.flux_pos = 0;
        self.flux_filled = 0;
        self.transients_hit = 0;
        for b in self.reset_bin.iter_mut() {
            *b = false;
        }
        if let Some(l) = self.low.as_mut() {
            l.reset(position.output_frame);
        }
        self.prev_source = 0.0;
        self.first_frame = true;
        for c in 0..ch {
            self.prev_phase[c][..bins].fill(0.0);
            self.cur_phase[c][..bins].fill(0.0);
            self.theta[c][..bins].fill(0.0);
        }
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
        // The low band looks a whole window further ahead than the main one,
        // and a caller sizing buffers needs the larger number, not the tidier
        // one (system-design.md sec.8).
        let look = self
            .low
            .as_ref()
            .map(|l| l.window_size())
            .unwrap_or(self.n)
            .max(self.n);
        LatencyInfo {
            lookahead_input_frames: look as u64,
            startup_padding_input_frames: 0,
            presentation_delay_output_frames: 0,
            tail_output_frames: 0,
        }
    }

    fn output_frames(&self) -> u64 {
        self.core.as_ref().map(|c| c.total_output()).unwrap_or(0)
    }

    fn input_position(&self) -> u64 {
        self.core.as_ref().map(|c| c.ring.end()).unwrap_or(0)
    }
}
