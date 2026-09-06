//! Tape: `y[t] = interpolate(x, W^-1(t))`.
//!
//! Pitch is tied to the map by construction, so an independent pitch request is
//! a conflict rather than something to approximate (system-design.md sec.6).
//! The read position is kept as a continuous `f64` and the kernel cutoff tracks
//! the local read speed, so speeding up does not fold energy back into the band
//! (dsp.md sec.3).

use super::core::{EngineCore, Stepper};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::dsp::resample::{SincBank, DEFAULT_HALF_TAPS, DEFAULT_OVERSAMPLE};

pub struct TapeEngine {
    core: Option<EngineCore>,
    bank: Option<SincBank>,
    context: usize,
}

impl Default for TapeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TapeEngine {
    pub fn new() -> Self {
        Self { core: None, bank: None, context: 0 }
    }
    pub const CAPABILITY: Capability = Capability {
        name: "tape",
        min_ratio: 0.05,
        max_ratio: 20.0,
        independent_pitch: false,
        formant_control: false,
        realtime_safe: true,
    };

    /// Semitones this map shifts the pitch by, for display. Only meaningful
    /// for a constant map.
    pub fn implied_semitones(cfg: &PreparedConfig) -> f64 {
        let (lo, hi) = cfg.map.ratio_range();
        let alpha = (lo + hi) * 0.5;
        -12.0 * alpha.log2()
    }
}

impl Stepper for TapeEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    fn step(&mut self) -> bool {
        let core = self.core.as_mut().expect("prepared");
        let bank = self.bank.as_ref().expect("prepared");
        let remaining = core.remaining_output();
        if remaining == 0 || core.out.space() == 0 {
            return false;
        }

        // How far ahead can we go before the source we would need is missing?
        let chunk = remaining.min(core.out.space() as u64).min(4096) as usize;
        let mut n = 0usize;
        while n < chunk {
            let t = (core.synth_pos + n as u64) as f64;
            let s = core.cfg.map.inverse(t);
            let from = s.floor() as i64 - self.context as i64;
            let to = s.ceil() as i64 + self.context as i64;
            if !core.source_ready(from, to) {
                break;
            }
            n += 1;
        }
        if n == 0 {
            return false;
        }

        let start = core.ring.start() as i64;
        for i in 0..n {
            let t = (core.synth_pos + i as u64) as f64;
            let s = core.cfg.map.inverse(t);
            let speed = core.cfg.map.speed_at_output(t);
            let cutoff = (1.0 / speed).min(1.0) as f32;
            let local = s - start as f64;
            for c in 0..core.cfg.channels {
                let plane = core.ring.plane(c);
                let v = bank.read(plane, local, cutoff);
                core.out.add(c, i, v);
            }
        }
        core.out.advance(n);
        core.synth_pos += n as u64;

        // Everything before the oldest position still needed can go.
        let next_s = core.cfg.map.inverse(core.synth_pos as f64);
        let keep_from = (next_s.floor() as i64 - self.context as i64).max(0) as u64;
        core.ring.discard_before(keep_from);
        true
    }
}

impl StretchEngine for TapeEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        if cfg.pitch != 1.0 {
            // Tape already determines pitch from the map.
            return Err(PrepareError::UnsupportedPitch {
                requested: cfg.pitch,
                min: 1.0,
                max: 1.0,
            });
        }
        if !cfg.formant.is_identity() {
            return Err(PrepareError::FormantNotSupported);
        }
        let (lo, hi) = cfg.map.ratio_range();
        let cap = Self::CAPABILITY;
        if lo < cap.min_ratio || hi > cap.max_ratio {
            return Err(PrepareError::UnsupportedRatio {
                requested: if lo < cap.min_ratio { lo } else { hi },
                min: cap.min_ratio,
                max: cap.max_ratio,
            });
        }
        let max_speed = 1.0 / lo.max(1e-6);
        let bank = SincBank::new(DEFAULT_HALF_TAPS, DEFAULT_OVERSAMPLE, max_speed);
        self.context = bank.context();
        // The ring has to hold the source span one output block can ask for.
        let span = (cfg.max_block as f64 * max_speed).ceil() as usize;
        let ring = (span + 2 * self.context + cfg.max_block).max(4096);
        let out = (cfg.max_block * 2).max(4096);
        self.bank = Some(bank);
        self.core = Some(EngineCore::new(cfg.clone(), ring, out));
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        if let Some(core) = self.core.as_mut() {
            let s = core.cfg.map.inverse(position.output_frame as f64);
            let start = (s.floor() as i64 - self.context as i64).max(0) as u64;
            core.reset_to(start);
            core.synth_pos = position.output_frame;
            core.delivered = position.output_frame;
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
        LatencyInfo {
            lookahead_input_frames: self.context as u64,
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
