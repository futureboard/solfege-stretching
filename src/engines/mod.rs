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
pub mod hybrid;
pub mod lowband;
pub mod percussive;
pub mod pitch;
pub mod pv;
pub mod tape;
pub mod texture;
pub mod wsola;

/// A source region that must not be stretched internally.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ProtectWindow {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
}

impl ProtectWindow {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
    pub fn contains(&self, frame: i64) -> bool {
        frame >= self.start as i64 && frame < self.end as i64
    }
}

/// Is `frame` inside any protected window?
///
/// Binary search, not a linear scan: this runs once per WSOLA search candidate,
/// and a busy track analyses to over a thousand onsets, which turns an innocent
/// `iter().any()` into hundreds of thousands of comparisons per frame.
/// `windows` must be sorted and merged, which is what `merge_protections`
/// guarantees.
#[inline]
pub fn protects(windows: &[ProtectWindow], frame: i64) -> bool {
    if windows.is_empty() || frame < 0 {
        return false;
    }
    let f = frame as u64;
    let i = windows.partition_point(|w| w.start <= f);
    i > 0 && windows[i - 1].end > f
}

/// Does any protected window *start* inside `[from, to)`? That is the question
/// a spectral engine asks: an attack beginning inside the analysis frame is
/// what triggers a phase reset.
#[inline]
pub fn protect_starts_in(windows: &[ProtectWindow], from: i64, to: i64) -> bool {
    if windows.is_empty() || to <= from {
        return false;
    }
    let lo = windows.partition_point(|w| (w.start as i64) < from);
    lo < windows.len() && (windows[lo].start as i64) < to
}

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
    /// Merged, sorted, in source frames.
    pub protections: Vec<ProtectWindow>,
    /// How long each attack is protected, in source frames.
    ///
    /// The spectral engines use it as a refractory period: once a transient has
    /// been re-anchored, the next `protect_frames` are treated as still being
    /// part of that attack rather than as new ones. Without it a long or
    /// multi-layered hit re-anchors several times in a row, and each one is
    /// another disagreement with the frames already written around it.
    pub protect_frames: u64,
    /// Is Attack Protect on at all?
    ///
    /// Separate from `protections` being empty, because the engines also detect
    /// transients themselves. Zero milliseconds of protection has to mean the
    /// whole mechanism is off - detector included - or the control has no
    /// setting that means "leave it alone".
    pub transient_protect: bool,
    /// Largest block the caller will ever pass to `process`.
    pub max_block: usize,
    pub seed: u64,
    /// STFT window for the spectral engines, in frames. `None` lets the engine
    /// pick from the quality profile and the sample rate.
    ///
    /// Window length is a real trade, not a tuning constant: a long window
    /// resolves bass partials that a short one smears into one bin, and blurs
    /// the attacks a short one keeps sharp (dsp.md sec.5). Exposing it means
    /// the choice can be measured instead of asserted.
    pub stft_size: Option<usize>,
    /// Window for the specialised low path. `Some(0)` disables the path
    /// entirely; `None` lets the compiler decide from the material.
    pub low_stft_size: Option<usize>,
    /// Hand the bass back to the main engine around transients.
    pub low_gate: bool,
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
                write!(f, "this engine has no formant path; pick Polyphonic or set FollowPitch")
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
