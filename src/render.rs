//! Offline rendering and the streaming adapter both engines share.
//!
//! The scheduler is what puts anchors in the right place; the reconciliation at
//! the end only settles a rounding difference that was defined up front. It is
//! never used to hide a cumulative timing bug, so a shortfall larger than a
//! frame is reported rather than padded away (system-design.md sec.12).

use crate::audio::{AudioBuffer, AudioView, AudioViewMut};
use crate::engines::{ProcessError, ProcessState, StretchEngine};
use crate::plan::{build_engine, PlanError, RenderPlan};
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum RenderError {
    Plan(PlanError),
    Process(ProcessError),
    /// The engine stopped short of `M` frames.
    Short { expected: u64, produced: u64 },
    /// The engine did not consume the whole source.
    UnconsumedInput { remaining: usize },
    /// The engine reported neither progress nor a reason.
    Stalled { at_output: u64 },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Plan(e) => write!(f, "{e}"),
            RenderError::Process(e) => write!(f, "{e}"),
            RenderError::Short { expected, produced } => {
                write!(f, "render produced {produced} of {expected} output frames")
            }
            RenderError::UnconsumedInput { remaining } => {
                write!(f, "{remaining} input frames were never consumed")
            }
            RenderError::Stalled { at_output } => {
                write!(f, "engine made no progress at output frame {at_output}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

impl From<PlanError> for RenderError {
    fn from(e: PlanError) -> Self {
        RenderError::Plan(e)
    }
}
impl From<ProcessError> for RenderError {
    fn from(e: ProcessError) -> Self {
        RenderError::Process(e)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderReport {
    pub expected_frames: u64,
    pub produced_frames: u64,
    pub consumed_frames: u64,
    pub process_calls: u64,
    pub peak: f32,
}

/// Render a whole source through a plan, feeding fixed-size blocks.
///
/// `block` is the caller's block size, not the engine's: the contract allows any
/// block size and the block-invariance gate depends on that being true.
pub fn render_offline(
    plan: &RenderPlan,
    source: &AudioBuffer,
    block: usize,
) -> Result<(AudioBuffer, RenderReport), RenderError> {
    let mut engine = build_engine(plan)?;
    render_with(&mut *engine, plan, source, block)
}

/// Same, with an engine the caller already prepared. Used by the tests that
/// need to inspect the engine afterwards.
pub fn render_with(
    engine: &mut dyn StretchEngine,
    plan: &RenderPlan,
    source: &AudioBuffer,
    block: usize,
) -> Result<(AudioBuffer, RenderReport), RenderError> {
    let expected = plan.output_frames();
    let channels = plan.cfg.channels;
    let block = block.clamp(1, plan.cfg.max_block);

    let mut out = AudioBuffer::silence(channels, expected as usize);
    let mut out_planes: Vec<Vec<f32>> = vec![vec![0.0; block]; channels];

    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut calls = 0u64;
    let mut consumed_total = 0usize;
    let total_in = source.frames();

    loop {
        let want_in = (total_in - in_pos).min(block);
        let end_of_input = in_pos + want_in >= total_in;
        let input = AudioView::from_planar(source.planes(), in_pos, want_in)
            .map_err(|_| RenderError::Stalled { at_output: out_pos as u64 })?;

        let room = (expected as usize - out_pos).min(block);
        let report = {
            let view = AudioViewMut::from_planar(&mut out_planes, 0, room)
                .map_err(|_| RenderError::Stalled { at_output: out_pos as u64 })?;
            engine.process(input, view, end_of_input)?
        };
        calls += 1;

        for c in 0..channels {
            let src = &out_planes[c][..report.produced_frames];
            out.channel_mut(c)[out_pos..out_pos + report.produced_frames].copy_from_slice(src);
        }
        in_pos += report.consumed_frames;
        consumed_total += report.consumed_frames;
        out_pos += report.produced_frames;

        if out_pos as u64 >= expected || report.state == ProcessState::Finished {
            break;
        }
        if report.consumed_frames == 0 && report.produced_frames == 0 {
            // The contract says a no-progress call must say what it waits for;
            // if it is waiting for input we have already delivered, it stalled.
            if report.state == ProcessState::NeedInput && in_pos >= total_in && end_of_input {
                return Err(RenderError::Stalled { at_output: out_pos as u64 });
            }
            if report.state == ProcessState::Draining || report.state == ProcessState::NeedInput {
                if room == 0 {
                    break;
                }
                return Err(RenderError::Stalled { at_output: out_pos as u64 });
            }
        }
        let call_budget = 8 * (expected / block as u64 + total_in as u64 / block as u64 + 64);
        if calls > call_budget {
            return Err(RenderError::Stalled { at_output: out_pos as u64 });
        }
    }

    if (out_pos as u64) < expected {
        return Err(RenderError::Short { expected, produced: out_pos as u64 });
    }
    if in_pos < total_in {
        return Err(RenderError::UnconsumedInput { remaining: total_in - in_pos });
    }

    let peak = out.peak();
    Ok((
        out,
        RenderReport {
            expected_frames: expected,
            produced_frames: out_pos as u64,
            consumed_frames: consumed_total as u64,
            process_calls: calls,
            peak,
        },
    ))
}

/// Feed blocks whose sizes come from a deterministic pseudo-random sequence.
/// The block-invariance gate compares this against a fixed block size.
pub fn render_offline_random_blocks(
    plan: &RenderPlan,
    source: &AudioBuffer,
    seed: u64,
    max_block: usize,
) -> Result<(AudioBuffer, RenderReport), RenderError> {
    let mut engine = build_engine(plan)?;
    let expected = plan.output_frames();
    let channels = plan.cfg.channels;
    let cap = max_block.clamp(1, plan.cfg.max_block);

    let mut out = AudioBuffer::silence(channels, expected as usize);
    let mut out_planes: Vec<Vec<f32>> = vec![vec![0.0; cap]; channels];
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) % cap as u64) as usize + 1
    };

    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut calls = 0u64;
    let total_in = source.frames();

    while (out_pos as u64) < expected {
        let want_in = (total_in - in_pos).min(next());
        let end_of_input = in_pos + want_in >= total_in;
        let input = AudioView::from_planar(source.planes(), in_pos, want_in)
            .map_err(|_| RenderError::Stalled { at_output: out_pos as u64 })?;
        let room = (expected as usize - out_pos).min(next());
        let report = {
            let view = AudioViewMut::from_planar(&mut out_planes, 0, room)
                .map_err(|_| RenderError::Stalled { at_output: out_pos as u64 })?;
            engine.process(input, view, end_of_input)?
        };
        calls += 1;
        for c in 0..channels {
            let src = &out_planes[c][..report.produced_frames];
            out.channel_mut(c)[out_pos..out_pos + report.produced_frames].copy_from_slice(src);
        }
        in_pos += report.consumed_frames;
        out_pos += report.produced_frames;
        if report.state == ProcessState::Finished {
            break;
        }
        if calls > 64 * (expected / 8 + 1024) {
            return Err(RenderError::Stalled { at_output: out_pos as u64 });
        }
    }

    if (out_pos as u64) < expected {
        return Err(RenderError::Short { expected, produced: out_pos as u64 });
    }
    let peak = out.peak();
    Ok((
        out,
        RenderReport {
            expected_frames: expected,
            produced_frames: out_pos as u64,
            consumed_frames: in_pos as u64,
            process_calls: calls,
            peak,
        },
    ))
}
