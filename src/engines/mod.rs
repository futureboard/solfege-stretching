//! The processing contract and the concrete engines.
//!
//! Every engine follows the same state machine and the same reporting rules
//! (system-design.md sec.7). `prepare` and `reset` may allocate and run outside
//! the audio callback; `process` may not allocate at all.

use crate::audio::{AudioError, AudioView, AudioViewMut};
use crate::document::{FormantPolicy, QualityProfile};
use crate::mapping::TimeMap;
use std::fmt;

pub mod bypass;
pub mod core;
pub mod elastic;
pub mod pitch;
pub mod soloist;
pub mod tape;
pub mod texture;

/// Everything an engine needs, fixed for the lifetime of one prepared plan.
#[derive(Clone, Debug)]
pub struct PreparedConfig {
    pub sample_rate: u32,
    pub channels: usize,
    /// Source frames -> output frames. Already validated.
    pub map: TimeMap,
    /// `p`, the pitch multiplier applied after the stretch stage.
    pub pitch: f64,
    pub formant: FormantPolicy,
    pub quality: QualityProfile,
    /// Transient handling on or off. The engines find transients themselves,
    /// from the audio, so the same plan behaves the same offline and live;
    /// this only says whether to act on them.
    pub transient_protect: bool,
    /// Largest block the caller will ever pass to `process`.
    pub max_block: usize,
    pub seed: u64,
    /// STFT window for the spectral engines, in frames. `None` lets the
    /// engine pick from its preset and the sample rate.
    pub stft_size: Option<usize>,
}

impl PreparedConfig {
    pub fn output_frames(&self) -> u64 {
        self.map.output_frames().get()
    }
    pub fn source_frames(&self) -> u64 {
        self.map.source_frames().get()
    }
    /// The stretch ratio the engine actually has to realise, pitch included.
    /// `alpha = 2, p = 2` means an internal 4x, which is what capability checks
    /// have to look at (system-design.md sec.6).
    pub fn internal_ratio_range(&self) -> (f64, f64) {
        let (lo, hi) = self.map.ratio_range();
        (lo * self.pitch, hi * self.pitch)
    }
}

/// Where a seek lands. Built outside the callback.
#[derive(Clone, Debug)]
pub struct PreparedSeek {
    pub output_frame: u64,
    /// Source frames of pre-roll the engine wants before the target.
    pub preroll: u64,
}

impl PreparedSeek {
    pub fn start() -> Self {
        Self { output_frame: 0, preroll: 0 }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ProcessState {
    /// Nothing more can be produced until more input arrives.
    NeedInput,
    /// Output was produced and more is ready without new input.
    HaveOutput,
    /// Input is finished; the tail is still coming.
    Draining,
    /// The whole output length has been delivered.
    Finished,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ProcessReport {
    pub consumed_frames: usize,
    pub produced_frames: usize,
    pub state: ProcessState,
    /// Logical output position of the first produced frame. There is no startup
    /// padding to subtract: these engines read the source by absolute position.
    pub output_start_frame: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LatencyInfo {
    /// Source frames the engine must see past the current read position.
    pub lookahead_input_frames: u64,
    /// Input frames consumed before the first output frame appears.
    pub startup_padding_input_frames: u64,
    /// Output frames of delay between a control change and hearing it.
    pub presentation_delay_output_frames: u64,
    /// Output frames still to come after the last input frame.
    pub tail_output_frames: u64,
}

impl LatencyInfo {
    pub const ZERO: Self = Self {
        lookahead_input_frames: 0,
        startup_padding_input_frames: 0,
        presentation_delay_output_frames: 0,
        tail_output_frames: 0,
    };
}

#[derive(Clone, Debug, PartialEq)]
pub enum PrepareError {
    Audio(AudioError),
    UnsupportedRatio { requested: f64, min: f64, max: f64 },
    UnsupportedPitch { requested: f64, min: f64, max: f64 },
    FormantNotSupported,
    BlockTooLarge { requested: usize, max: usize },
    EmptyPlan,
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrepareError::Audio(e) => write!(f, "{e}"),
            PrepareError::UnsupportedRatio { requested, min, max } => write!(
                f,
                "internal stretch ratio {requested:.4} is outside this engine's range {min}..{max}"
            ),
            PrepareError::UnsupportedPitch { requested, min, max } => write!(
                f,
                "pitch multiplier {requested:.4} is outside this engine's range {min}..{max}"
            ),
            PrepareError::FormantNotSupported => {
                write!(f, "this engine has no formant path; pick an Elastic mode or Soloist")
            }
            PrepareError::BlockTooLarge { requested, max } => {
                write!(f, "block size {requested} exceeds the prepared maximum {max}")
            }
            PrepareError::EmptyPlan => write!(f, "nothing to render"),
        }
    }
}

impl std::error::Error for PrepareError {}

impl From<AudioError> for PrepareError {
    fn from(e: AudioError) -> Self {
        PrepareError::Audio(e)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcessError {
    NotPrepared,
    ChannelMismatch { expected: usize, found: usize },
    BlockTooLarge { requested: usize, max: usize },
    InputAfterEndOfInput,
    NonFiniteInput { channel: usize, frame: usize },
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessError::NotPrepared => write!(f, "process() before prepare()"),
            ProcessError::ChannelMismatch { expected, found } => {
                write!(f, "expected {expected} channels, got {found}")
            }
            ProcessError::BlockTooLarge { requested, max } => {
                write!(f, "block size {requested} exceeds the prepared maximum {max}")
            }
            ProcessError::InputAfterEndOfInput => {
                write!(f, "new input after end-of-input; reset() first")
            }
            ProcessError::NonFiniteInput { channel, frame } => {
                write!(f, "non-finite input at channel {channel}, frame {frame}")
            }
        }
    }
}

impl std::error::Error for ProcessError {}

/// What an engine can and cannot do. The plan compiler reads this before it
/// commits to a mode, so a rejection happens before any audio is rendered.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Capability {
    pub name: &'static str,
    pub min_ratio: f64,
    pub max_ratio: f64,
    pub independent_pitch: bool,
    pub formant_control: bool,
    pub realtime_safe: bool,
}

pub trait StretchEngine: Send {
    fn capability(&self) -> Capability;

    /// May allocate. Never called from the audio callback.
    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError>;

    /// May allocate. Never called from the audio callback.
    fn reset(&mut self, position: &PreparedSeek);

    /// Allocation-free. Consumes a prefix of `input`, fills a prefix of
    /// `output`, and reports exactly what it did.
    fn process(
        &mut self,
        input: AudioView<'_>,
        output: AudioViewMut<'_>,
        end_of_input: bool,
    ) -> Result<ProcessReport, ProcessError>;

    fn latency(&self) -> LatencyInfo;

    /// Total output frames this plan will deliver.
    fn output_frames(&self) -> u64;

    /// The absolute source frame the engine expects next.
    ///
    /// A pull adapter has to feed from exactly here: the input ring indexes by
    /// absolute source position, so starting a feed one frame early or late
    /// would silently shift the whole source under the map. After `reset` this
    /// is where that seek landed, not zero.
    fn input_position(&self) -> u64;

    /// Switch a running engine to a new map, pitch and formant without
    /// rebuilding it, keeping its phase state and source position.
    ///
    /// Returns the logical output position of the next frame to be
    /// delivered, in the new map's coordinates, or `None` if this engine
    /// cannot be retargeted (the caller then builds a new one). On success
    /// `map` holds the *old* map so the caller can drop it away from the
    /// audio thread. Must not allocate or free.
    fn retarget(
        &mut self,
        map: &mut TimeMap,
        pitch: f64,
        formant: FormantPolicy,
    ) -> Option<u64> {
        let _ = (map, pitch, formant);
        None
    }
}

/// Frames of source context an engine wants behind and ahead of the read
/// position. Used to size the input ring and to compute seek pre-roll.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub back: usize,
    pub forward: usize,
}

impl Context {
    pub const NONE: Self = Self { back: 0, forward: 0 };
}
