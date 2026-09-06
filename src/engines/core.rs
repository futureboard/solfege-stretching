//! The state machine every engine shares.
//!
//! An engine only has to say how to synthesise the next grain; the loop below
//! owns the contract: consume a prefix, produce a prefix, latch EOF once the
//! last input frame is actually consumed, never spin without saying what it is
//! waiting for, and stop at exactly `M` output frames (system-design.md sec.7
//! and sec.12).

use super::{PreparedConfig, ProcessError, ProcessReport, ProcessState};
use crate::audio::{AudioView, AudioViewMut};
use crate::runtime::{InputRing, OutputAccum};

pub struct EngineCore {
    pub cfg: PreparedConfig,
    pub ring: InputRing,
    pub out: OutputAccum,
    /// Output frames already synthesised into `out` (final, not yet drained).
    pub synth_pos: u64,
    /// Output frames handed to the caller.
    pub delivered: u64,
    /// Latched once the caller's last input frame has been consumed.
    pub eof: bool,
    pub prepared: bool,
}

impl EngineCore {
    pub fn new(cfg: PreparedConfig, ring_capacity: usize, out_capacity: usize) -> Self {
        let ch = cfg.channels;
        Self {
            ring: InputRing::new(ch, ring_capacity),
            out: OutputAccum::new(ch, out_capacity),
            cfg,
            synth_pos: 0,
            delivered: 0,
            eof: false,
            prepared: true,
        }
    }

    pub fn total_output(&self) -> u64 {
        self.cfg.output_frames()
    }

    pub fn remaining_output(&self) -> u64 {
        self.total_output().saturating_sub(self.synth_pos)
    }

    /// True when the source range `[from, to)` is either in the ring or past
    /// the end of a source we have already fully consumed (defined as zero
    /// padding, dsp.md sec.4 requirement 4).
    pub fn source_ready(&self, from: i64, to: i64) -> bool {
        let n = self.cfg.source_frames() as i64;
        let need_to = to.min(n);
        if need_to <= from {
            return true;
        }
        if self.ring.holds(from.max(0), need_to) {
            return true;
        }
        // After EOF no more source is coming, so anything still missing is the
        // defined zero padding rather than a reason to wait forever.
        self.eof
    }

    pub fn reset_to(&mut self, source_start: u64) {
        self.ring.reset(source_start);
        self.out.reset();
        self.eof = false;
    }
}

/// Implemented by each engine; `drive` turns it into the full contract.
pub trait Stepper {
    fn core(&mut self) -> &mut EngineCore;
    fn core_ref(&self) -> &EngineCore;

    /// Synthesise the next grain into `core.out`. Returns true only when it
    /// actually advanced the write head, so the driver can never spin.
    fn step(&mut self) -> bool;

    fn drive(
        &mut self,
        input: AudioView<'_>,
        mut output: AudioViewMut<'_>,
        end_of_input: bool,
    ) -> Result<ProcessReport, ProcessError> {
        {
            let c = self.core_ref();
            if !c.prepared {
                return Err(ProcessError::NotPrepared);
            }
            if input.frames() > 0 {
                if input.channel_count() != c.cfg.channels {
                    return Err(ProcessError::ChannelMismatch {
                        expected: c.cfg.channels,
                        found: input.channel_count(),
                    });
                }
                if input.frames() > c.cfg.max_block {
                    return Err(ProcessError::BlockTooLarge {
                        requested: input.frames(),
                        max: c.cfg.max_block,
                    });
                }
                if c.eof {
                    return Err(ProcessError::InputAfterEndOfInput);
                }
            }
        }

        let output_start_frame = self.core_ref().delivered;
        let mut consumed = 0usize;
        let mut produced = 0usize;

        loop {
            let took = {
                let c = self.core();
                c.out.compact();
                c.ring.push(&input, consumed)
            };
            consumed += took;
            {
                let c = self.core();
                if end_of_input && consumed >= input.frames() {
                    c.eof = true;
                }
            }

            let made = self.step();

            let drained = {
                let c = self.core();
                let n = c.out.drain_into(&mut output, produced);
                c.delivered += n as u64;
                n
            };
            produced += drained;

            if produced >= output.capacity() {
                break;
            }
            if took == 0 && !made && drained == 0 {
                break;
            }
        }

        let c = self.core_ref();
        let state = if c.delivered >= c.total_output() {
            ProcessState::Finished
        } else if c.out.pending() > 0 {
            ProcessState::HaveOutput
        } else if c.eof {
            ProcessState::Draining
        } else {
            ProcessState::NeedInput
        };

        Ok(ProcessReport {
            consumed_frames: consumed,
            produced_frames: produced,
            state,
            output_start_frame,
        })
    }
}
