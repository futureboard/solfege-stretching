//! Texture: granular resynthesis, an FX engine.
//!
//! This is not a fallback for the natural modes. It repeats and scatters grains,
//! which changes the texture on purpose; the docs are explicit that a Natural
//! render must not quietly land here (research.md sec.9, dsp.md sec.7).
//!
//! Grain jitter is driven by a seeded PRNG from the edit document, so the same
//! document renders the same audio every time (system-design.md sec.10).

use super::core::{EngineCore, Stepper};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::dsp::window::hann_periodic;
use crate::runtime::OutputAccum;

/// xorshift64*, seeded per render.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `-1..1`.
    fn bipolar(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

pub struct TextureEngine {
    core: Option<EngineCore>,
    window: Vec<f32>,
    grain: usize,
    hop: usize,
    spread: f64,
    rng: Rng,
    seed: u64,
}

impl Default for TextureEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextureEngine {
    pub fn new() -> Self {
        Self {
            core: None,
            window: Vec::new(),
            grain: 0,
            hop: 0,
            spread: 0.0,
            rng: Rng::new(0),
            seed: 0,
        }
    }

    pub const CAPABILITY: Capability = Capability {
        name: "texture",
        min_ratio: 0.05,
        max_ratio: 100.0,
        independent_pitch: false,
        formant_control: false,
        realtime_safe: true,
    };
}

impl Stepper for TextureEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    fn step(&mut self) -> bool {
        let core = self.core.as_mut().expect("prepared");
        let remaining = core.remaining_output();
        if remaining == 0 || core.out.space() < self.grain {
            return false;
        }
        let t = core.synth_pos as f64;
        let centre = core.cfg.map.inverse(t);
        let jitter = self.rng.bipolar() * self.spread;
        let s = (centre + jitter).max(0.0).round() as i64;
        if !core.source_ready(s, s + self.grain as i64) {
            return false;
        }
        for c in 0..core.cfg.channels {
            for i in 0..self.grain {
                let v = core.ring.at(c, s + i as i64);
                core.out.add(c, i, v * self.window[i]);
            }
        }
        for i in 0..self.grain {
            core.out.add_norm(i, self.window[i]);
        }
        let advance = self.hop.min(remaining as usize);
        core.out.advance_normalized(advance, 1e-3);
        core.synth_pos += advance as u64;

        let next = core.cfg.map.inverse(core.synth_pos as f64) - self.spread;
        core.ring.discard_before(next.max(0.0) as u64);
        true
    }
}

impl StretchEngine for TextureEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        let sr = cfg.sample_rate as f64;
        let mut grain = (sr * 0.080).round() as usize;
        grain += grain % 2;
        self.grain = grain;
        self.hop = grain / 2;
        self.window = hann_periodic(grain);
        self.spread = sr * 0.020;
        self.seed = cfg.seed;
        self.rng = Rng::new(cfg.seed);

        let (lo, _) = cfg.map.ratio_range();
        let max_speed = 1.0 / lo.max(1e-6);
        let span = (cfg.max_block as f64 * max_speed).ceil() as usize;
        let ring = span + grain + 2 * self.spread as usize + cfg.max_block + 64;
        let out_cap = (cfg.max_block + 4 * grain).max(8192);
        let mut core = EngineCore::new(cfg.clone(), ring, out_cap);
        core.out = OutputAccum::with_norm(cfg.channels, out_cap);
        self.core = Some(core);
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        self.rng = Rng::new(self.seed ^ position.output_frame);
        if let Some(core) = self.core.as_mut() {
            let s = core.cfg.map.inverse(position.output_frame as f64);
            let start = (s - self.spread).max(0.0) as u64;
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
            lookahead_input_frames: (self.grain as f64 + self.spread) as u64,
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
