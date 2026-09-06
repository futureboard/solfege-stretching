//! Transposition stage: stretch by `alpha*p`, then read back at rate `p`.
//!
//! ```text
//! u(t) = integral[0..t] p(q) dq      (p constant here: u(t) = p*t)
//! U(s) = u(W(s))                    -> the map the inner engine renders
//! z    = stretch(x, U)
//! y(t) = resample(z, u(t))
//! ```
//!
//! The two clocks are kept apart on purpose: the inner engine works in
//! intermediate time, this stage works in output time, and the only place they
//! meet is `u`. `alpha*p` is never handed to a stage that thinks in the other
//! clock (dsp.md sec.3).
//!
//! The inner engine also gets the exact intermediate endpoint, so the anchors
//! still land after resampling instead of drifting by a rounding error.

use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, ProcessState, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::dsp::resample::{SincBank, DEFAULT_HALF_TAPS, DEFAULT_OVERSAMPLE};
use crate::mapping::{TimeMap, WarpAnchor};
use crate::runtime::InputRing;

pub struct PitchStage {
    inner: Box<dyn StretchEngine>,
    bank: Option<SincBank>,
    /// Intermediate signal, indexed by intermediate output frame.
    z: InputRing,
    /// Scratch the inner engine writes into before it reaches `z`.
    scratch: Vec<Vec<f32>>,
    pitch: f64,
    context: usize,
    channels: usize,
    output_frames: u64,
    intermediate_frames: u64,
    produced: u64,
    inner_done: bool,
    prepared: bool,
    max_block: usize,
}

impl PitchStage {
    pub fn new(inner: Box<dyn StretchEngine>, pitch: f64) -> Self {
        Self {
            inner,
            bank: None,
            z: InputRing::new(1, 1),
            scratch: Vec::new(),
            pitch,
            context: 0,
            channels: 1,
            output_frames: 0,
            intermediate_frames: 0,
            produced: 0,
            inner_done: false,
            prepared: false,
            max_block: 0,
        }
    }

    /// The map the inner engine renders: output time scaled into intermediate
    /// time by `p`. Anchors keep their source positions.
    pub fn intermediate_map(map: &TimeMap, pitch: f64) -> TimeMap {
        if map.is_empty() {
            return TimeMap::empty();
        }
        let mut anchors: Vec<WarpAnchor> = map
            .anchors()
            .iter()
            .map(|a| {
                WarpAnchor::new(
                    a.source_frame,
                    ((a.output_frame as f64) * pitch).round().max(0.0) as u64,
                    a.kind,
                )
            })
            .collect();
        // Repair a collision the rounding could have created; the source axis
        // is untouched, so only the output axis can collapse.
        for i in 1..anchors.len() {
            if anchors[i].output_frame <= anchors[i - 1].output_frame {
                anchors[i].output_frame = anchors[i - 1].output_frame + 1;
            }
        }
        TimeMap::new(anchors).unwrap_or_else(|_| TimeMap::empty())
    }

    pub fn inner(&self) -> &dyn StretchEngine {
        self.inner.as_ref()
    }
}

impl StretchEngine for PitchStage {
    fn capability(&self) -> Capability {
        let inner = self.inner.capability();
        Capability { independent_pitch: true, ..inner }
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        if !cfg.pitch.is_finite() || cfg.pitch <= 0.0 {
            return Err(PrepareError::UnsupportedPitch {
                requested: cfg.pitch,
                min: 0.25,
                max: 4.0,
            });
        }
        let p = cfg.pitch;
        self.pitch = p;
        let inner_map = Self::intermediate_map(&cfg.map, p);
        let mut inner_cfg = cfg.clone();
        inner_cfg.map = inner_map;
        // The inner engine stretches only; it still needs `pitch` and the
        // formant policy so the polyphonic path can pre-compensate the
        // envelope for the resampling that happens here.
        inner_cfg.max_block = cfg.max_block.max(1024);
        self.inner.prepare(&inner_cfg)?;

        let bank = SincBank::new(DEFAULT_HALF_TAPS, DEFAULT_OVERSAMPLE, p.max(1.0));
        self.context = bank.context();
        self.channels = cfg.channels;
        self.output_frames = cfg.output_frames();
        self.intermediate_frames = inner_cfg.output_frames();
        self.max_block = cfg.max_block;

        let block = cfg.max_block.max(1024);
        let z_span = ((block as f64) * p).ceil() as usize + 2 * self.context + block + 64;
        self.z = InputRing::new(cfg.channels, z_span);
        self.scratch = vec![vec![0.0; block]; cfg.channels];
        self.bank = Some(bank);
        self.produced = 0;
        self.inner_done = false;
        self.prepared = true;
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        let inner_pos = PreparedSeek {
            output_frame: ((position.output_frame as f64) * self.pitch).round() as u64,
            preroll: position.preroll,
        };
        self.inner.reset(&inner_pos);
        // The ring is labelled with the position of the first frame the inner
        // engine will actually produce, not that position minus the kernel
        // context. Backing it off by the context used to leave the ring
        // claiming to start earlier than any data it would ever receive, and
        // whether that mattered came down to which way `output_frame * p`
        // rounded: half the time the resampler's readiness check could never be
        // satisfied, the ring filled, the inner engine stopped being called,
        // and the whole stage deadlocked producing nothing. Offline never saw
        // it because it never seeks, and a seek to zero rounds exactly.
        self.z.reset(inner_pos.output_frame);
        self.produced = position.output_frame;
        self.inner_done = false;
    }

    fn process(
        &mut self,
        input: AudioView<'_>,
        mut output: AudioViewMut<'_>,
        end_of_input: bool,
    ) -> Result<ProcessReport, ProcessError> {
        if !self.prepared {
            return Err(ProcessError::NotPrepared);
        }
        if input.frames() > self.max_block {
            return Err(ProcessError::BlockTooLarge {
                requested: input.frames(),
                max: self.max_block,
            });
        }
        let bank = self.bank.as_ref().expect("prepared");
        let output_start_frame = self.produced;
        let mut consumed = 0usize;
        let mut written = 0usize;

        loop {
            // 1. pull the intermediate signal forward
            let mut inner_made = 0usize;
            if !self.inner_done && self.z.space() > 0 {
                let want = self.z.space().min(self.scratch[0].len());
                let remaining = input.sub(consumed);
                let report = {
                    let view = AudioViewMut::from_planar(&mut self.scratch, 0, want)
                        .map_err(|_| ProcessError::NotPrepared)?;
                    self.inner.process(remaining, view, end_of_input)?
                };
                consumed += report.consumed_frames;
                inner_made = report.produced_frames;
                if inner_made > 0 {
                    let z_in = AudioView::from_planar(&self.scratch, 0, inner_made)
                        .map_err(|_| ProcessError::NotPrepared)?;
                    self.z.push(&z_in, 0);
                }
                if report.state == ProcessState::Finished {
                    self.inner_done = true;
                }
            }

            // 2. resample from intermediate time into output time
            let cutoff = (1.0 / self.pitch).min(1.0) as f32;
            let mut made = 0usize;
            while written + made < output.capacity()
                && self.produced + (made as u64) < self.output_frames
            {
                let t = (self.produced + made as u64) as f64;
                let u = t * self.pitch;
                let from = u.floor() as i64 - self.context as i64;
                let to = u.ceil() as i64 + self.context as i64;
                // Kernel context that reaches before the first produced frame
                // or past the last one is the defined zero padding, not a
                // reason to wait - the same rule the engines use for source.
                // Kernel context that reaches before the first produced frame
                // or past the last one is the defined zero padding, not a
                // reason to wait - the same rule the engines use for source.
                // Without this the readiness test could never be satisfied
                // after a seek whose rounding went the wrong way, the ring
                // filled, and the stage deadlocked.
                let need_from = from.max(self.z.start() as i64);
                let need_to = to.min(self.intermediate_frames as i64);
                let have = need_to <= need_from
                    || self.z.holds(need_from, need_to)
                    || self.inner_done;
                if !have {
                    break;
                }
                let local = u - self.z.start() as f64;
                for c in 0..self.channels {
                    let plane = self.z.plane(c);
                    let v = bank.read(plane, local, cutoff);
                    output.channel_mut(c)[written + made] = v;
                }
                made += 1;
            }
            self.produced += made as u64;
            written += made;

            if made > 0 {
                let next_u = (self.produced as f64) * self.pitch;
                let keep = (next_u.floor() as i64 - self.context as i64).max(0) as u64;
                self.z.discard_before(keep);
            }

            if written >= output.capacity() {
                break;
            }
            if inner_made == 0 && made == 0 {
                break;
            }
        }

        let state = if self.produced >= self.output_frames {
            ProcessState::Finished
        } else if self.inner_done {
            ProcessState::Draining
        } else {
            ProcessState::NeedInput
        };

        Ok(ProcessReport {
            consumed_frames: consumed,
            produced_frames: written,
            state,
            output_start_frame,
        })
    }

    fn latency(&self) -> LatencyInfo {
        let inner = self.inner.latency();
        LatencyInfo {
            lookahead_input_frames: inner.lookahead_input_frames,
            startup_padding_input_frames: inner.startup_padding_input_frames,
            presentation_delay_output_frames: inner.presentation_delay_output_frames
                + (self.context as f64 / self.pitch).ceil() as u64,
            tail_output_frames: inner.tail_output_frames,
        }
    }

    fn output_frames(&self) -> u64 {
        self.output_frames
    }

    fn input_position(&self) -> u64 {
        // The stretch stage is what touches the source; this stage only reads
        // the intermediate signal.
        self.inner.input_position()
    }
}
