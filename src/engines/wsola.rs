//! WSOLA: waveform-similarity overlap-add.
//!
//! Frames are placed on a fixed synthesis grid and the analysis position is
//! allowed to slide within a search window so the waveform joins cleanly. The
//! nominal analysis position is recomputed from the map every frame, so a
//! search offset can never accumulate into drift, and the search width is
//! narrowed to zero near a hard anchor so an anchor still lands where the user
//! put it (dsp.md sec.4).
//!
//! This engine needs no pitch detector. Transposition is composed by the pitch
//! stage on top; calling it PSOLA would be wrong.

use super::core::{EngineCore, Stepper};
use super::{
    Capability, Context, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::document::QualityProfile;
use super::protects;
use crate::dsp::corr::best_offset;
use crate::dsp::window::hann_periodic;
use crate::mapping::AnchorKind;
use crate::runtime::OutputAccum;

pub struct WsolaEngine {
    core: Option<EngineCore>,
    /// Full analysis/synthesis window, length `n`.
    window: Vec<f32>,
    /// `window` with a flat first half, for the very first frame.
    window_flat_start: Vec<f32>,
    /// `window` with a flat second half, for the final frame.
    window_flat_tail: Vec<f32>,
    n: usize,
    hop: usize,
    max_shift: i64,
    /// Stride of the coarse correlation pass.
    search_decimate: usize,
    /// Preallocated correlation target, `channels x hop`.
    target: Vec<Vec<f32>>,
    /// Source start of the frame placed last, or `None` before the first.
    prev_source: Option<f64>,
    first_frame: bool,
    /// Interior anchor output positions, for narrowing the search.
    anchor_outputs: Vec<u64>,
    last_score: f64,
}

impl Default for WsolaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WsolaEngine {
    pub fn new() -> Self {
        Self {
            core: None,
            window: Vec::new(),
            window_flat_start: Vec::new(),
            window_flat_tail: Vec::new(),
            n: 0,
            hop: 0,
            max_shift: 0,
            search_decimate: 1,
            target: Vec::new(),
            prev_source: None,
            first_frame: true,
            anchor_outputs: Vec::new(),
            last_score: 0.0,
        }
    }

    pub const CAPABILITY: Capability = Capability {
        name: "wsola",
        min_ratio: 0.25,
        max_ratio: 4.0,
        independent_pitch: false,
        formant_control: false,
        realtime_safe: true,
    };

    /// Correlation score of the frame placed last: a cheap health signal for
    /// the UI, not a quality measurement.
    pub fn last_score(&self) -> f64 {
        self.last_score
    }

    pub fn context(&self) -> Context {
        Context { back: self.max_shift as usize, forward: self.n + self.max_shift as usize }
    }

}

/// Search half-width at an output position. Shrinks to zero as a hard anchor
/// approaches so the anchor is not smeared by the search.
///
/// Only the nearest anchor can constrain the limit, so this binary-searches a
/// sorted list instead of walking it: promoting every detected onset on a busy
/// track puts thousands of anchors here, once per synthesis frame. Free
/// function so it does not borrow all of the engine while `core` is borrowed
/// mutably.
fn shift_limit_at(anchors: &[u64], t: u64, n: usize, max_shift: i64) -> i64 {
    if anchors.is_empty() {
        return max_shift;
    }
    let i = anchors.partition_point(|a| *a < t);
    let mut nearest = u64::MAX;
    if i < anchors.len() {
        nearest = nearest.min(anchors[i] - t);
    }
    if i > 0 {
        nearest = nearest.min(t - anchors[i - 1]);
    }
    if nearest >= n as u64 {
        return max_shift;
    }
    let scale = nearest as f64 / n as f64;
    ((max_shift as f64 * scale).floor() as i64).max(0)
}

impl Stepper for WsolaEngine {
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
        if core.out.space() < self.n {
            return false;
        }

        let t = core.synth_pos;
        let s_nom = core.cfg.map.inverse(t as f64).round() as i64;
        let limit = shift_limit_at(&self.anchor_outputs, t, self.n, self.max_shift);
        let last_frame = remaining <= self.n as u64;

        // Everything the search plus the frame could touch must be available.
        let need_from = s_nom - limit;
        let need_to = s_nom + self.n as i64 + limit;
        if !core.source_ready(need_from, need_to) {
            return false;
        }

        // Where does the previous frame naturally continue?
        let shift = if self.first_frame || limit == 0 {
            0
        } else {
            let prev = self.prev_source.unwrap_or(s_nom as f64);
            let cont = prev.round() as i64 + self.hop as i64;
            for c in 0..core.cfg.channels {
                for i in 0..self.hop {
                    self.target[c][i] = core.ring.at(c, cont + i as i64);
                }
            }
            let ring = &core.ring;
            let protections = &core.cfg.protections;
            let res = best_offset(
                core.cfg.channels,
                |c, f| ring.at(c, f),
                &self.target,
                s_nom,
                self.hop,
                limit,
                self.search_decimate,
                // Never start a frame inside a protected attack: that is how a
                // hit gets retriggered.
                |d| !protects(protections, s_nom + d),
            );
            self.last_score = res.score;
            if res.silent {
                0
            } else {
                res.offset
            }
        };

        let s_m = s_nom + shift;
        let win: &[f32] = if self.first_frame {
            &self.window_flat_start
        } else if last_frame {
            &self.window_flat_tail
        } else {
            &self.window
        };

        for c in 0..core.cfg.channels {
            for i in 0..self.n {
                let v = core.ring.at(c, s_m + i as i64);
                core.out.add(c, i, v * win[i]);
            }
        }
        for i in 0..self.n {
            core.out.add_norm(i, win[i]);
        }

        let advance = self.hop.min(remaining as usize);
        core.out.advance_normalized(advance, 1e-3);
        core.synth_pos += advance as u64;
        self.prev_source = Some(s_m as f64);
        self.first_frame = false;

        // Keep enough history for the next search, drop the rest.
        let next_s = core.cfg.map.inverse(core.synth_pos as f64);
        let keep = (next_s.floor() as i64 - self.max_shift - 1).max(0) as u64;
        core.ring.discard_before(keep);
        true
    }
}

impl StretchEngine for WsolaEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
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

        let sr = cfg.sample_rate as f64;
        let frame_ms = match cfg.quality {
            QualityProfile::Offline => 30.0,
            QualityProfile::Realtime => 24.0,
        };
        let mut n = (sr * frame_ms / 1000.0).round() as usize;
        n += n % 2;
        let hop = n / 2;
        let max_shift = (sr * 5.0 / 1000.0).round() as i64;

        self.window = hann_periodic(n);
        self.window_flat_start = self.window.clone();
        for w in self.window_flat_start[..hop].iter_mut() {
            *w = 1.0;
        }
        self.window_flat_tail = self.window.clone();
        for w in self.window_flat_tail[hop..].iter_mut() {
            *w = 1.0;
        }
        self.n = n;
        self.hop = hop;
        self.max_shift = max_shift;
        // Keep the coarse pass near 192 taps whatever the frame length is: fine
        // enough to lock onto a pitch period, cheap enough for a callback.
        self.search_decimate = (hop / 192).max(1);
        self.target = vec![vec![0.0; hop]; cfg.channels];
        self.prev_source = None;
        self.first_frame = true;
        self.last_score = 0.0;
        // Sorted, because `shift_limit_at` binary-searches it. The map keeps
        // anchors in source order and the output axis is monotonic with it, so
        // this is already ascending; sort anyway rather than rely on that.
        self.anchor_outputs = cfg
            .map
            .anchors()
            .iter()
            .filter(|a| a.kind != AnchorKind::Endpoint)
            .map(|a| a.output_frame)
            .collect();
        self.anchor_outputs.sort_unstable();

        // Ring: the source span of one block, plus frame and search context.
        let max_speed = 1.0 / lo.max(1e-6);
        let span = (cfg.max_block as f64 * max_speed).ceil() as usize;
        let ring = span + n + 2 * max_shift as usize + cfg.max_block + 64;
        let out_cap = (cfg.max_block + 4 * n).max(4096);
        let mut core = EngineCore::new(cfg.clone(), ring, out_cap);
        core.out = OutputAccum::with_norm(cfg.channels, out_cap);
        self.core = Some(core);
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        if let Some(core) = self.core.as_mut() {
            let s = core.cfg.map.inverse(position.output_frame as f64);
            let start = (s.floor() as i64 - self.max_shift - 1).max(0) as u64;
            core.reset_to(start);
            core.synth_pos = position.output_frame;
            core.delivered = position.output_frame;
        }
        self.prev_source = None;
        self.first_frame = true;
        self.last_score = 0.0;
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
            lookahead_input_frames: (self.n as i64 + self.max_shift) as u64,
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
