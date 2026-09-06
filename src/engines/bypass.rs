//! Identity copy.
//!
//! This is the engine the identity gate measures: with no edits at all the
//! output must be bit-identical to the input in the internal float path
//! (validation.md sec.4). It therefore does no arithmetic on the samples.

use super::core::{EngineCore, Stepper};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};

pub struct BypassEngine {
    core: Option<EngineCore>,
}

impl Default for BypassEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl BypassEngine {
    pub fn new() -> Self {
        Self { core: None }
    }
    pub const CAPABILITY: Capability = Capability {
        name: "bypass",
        min_ratio: 1.0,
        max_ratio: 1.0,
        independent_pitch: false,
        formant_control: false,
        realtime_safe: true,
    };
}

impl Stepper for BypassEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    fn step(&mut self) -> bool {
        let core = self.core.as_mut().expect("prepared");
        let remaining = core.remaining_output();
        if remaining == 0 {
            return false;
        }
        let start = core.synth_pos as i64;
        let avail = (core.ring.end() as i64 - start).max(0) as u64;
        let n = avail.min(remaining).min(core.out.space() as u64) as usize;
        if n == 0 {
            return false;
        }
        for c in 0..core.cfg.channels {
            for i in 0..n {
                let v = core.ring.at(c, start + i as i64);
                core.out.add(c, i, v);
            }
        }
        core.out.advance(n);
        core.synth_pos += n as u64;
        core.ring.discard_before(core.synth_pos);
        true
    }
}

impl StretchEngine for BypassEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        if !cfg.map.is_empty() && !cfg.map.is_identity() {
            let (lo, hi) = cfg.map.ratio_range();
            return Err(PrepareError::UnsupportedRatio {
                requested: if (lo - 1.0).abs() > (hi - 1.0).abs() { lo } else { hi },
                min: 1.0,
                max: 1.0,
            });
        }
        if cfg.pitch != 1.0 {
            return Err(PrepareError::UnsupportedPitch {
                requested: cfg.pitch,
                min: 1.0,
                max: 1.0,
            });
        }
        let ring = (cfg.max_block * 2).max(1024);
        let out = (cfg.max_block * 2).max(1024);
        self.core = Some(EngineCore::new(cfg.clone(), ring, out));
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        if let Some(core) = self.core.as_mut() {
            let s = position.output_frame;
            core.reset_to(s);
            core.synth_pos = s;
            core.delivered = s;
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
        LatencyInfo::ZERO
    }

    fn output_frames(&self) -> u64 {
        self.core.as_ref().map(|c| c.total_output()).unwrap_or(0)
    }

    fn input_position(&self) -> u64 {
        self.core.as_ref().map(|c| c.ring.end()).unwrap_or(0)
    }
}
