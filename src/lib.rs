//! Solfege Stretching — time stretching, pitch shifting and warping.
//!
//! The library is organised the way `docs/system-design.md` describes it:
//!
//! ```text
//! immutable source ─┐
//!                   ├─> analysis ─> plan compiler ─> render plan ─> engine
//! edit document ────┘                                                 │
//!                                          pitch stage + delay align ─┘
//! ```
//!
//! Two consumers sit on the end of that chain: [`render`] for offline export,
//! and [`stream`] for real-time playback, where the audio callback runs the
//! same engine under a work budget while a worker feeds it.
//!
//! Three invariants run through everything and are worth stating once:
//!
//! * **The map is the contract.** Anchors are sample boundaries, the map is
//!   strictly monotonic, and an offline render delivers exactly `M` frames.
//! * **Nothing is repaired silently.** An infeasible transient constraint, an
//!   out-of-range internal ratio or a formant request on an engine with no
//!   formant path is a typed error before rendering starts.
//! * **Channels are decided together.** One search offset, one peak partition,
//!   one transient decision, applied to every channel.
//!
//! What this crate does *not* claim: parity with any commercial engine. The
//! quality gates in `docs/validation.md` are targets, and the numbers only mean
//! something once they have been measured.

pub mod analysis;
pub mod audio;
pub mod document;
pub mod dsp;
pub mod engines;
pub mod fixtures;
pub mod mapping;
pub mod metrics;
pub mod plan;
pub mod render;
pub mod runtime;
pub mod stream;
pub mod wav;

pub use audio::{AudioBuffer, AudioView, AudioViewMut, OutputFrame, SourceFrame, SourceIdentity};
pub use document::{EditDocument, EngineMode, FormantPolicy, NoteEdit, QualityProfile};
pub use engines::{LatencyInfo, ProcessReport, ProcessState, StretchEngine};
pub use mapping::{AnchorKind, TimeMap, WarpAnchor};
pub use plan::{compile, build_engine, CompileOptions, PlanError, RenderPlan};
pub use render::{render_offline, RenderError, RenderReport};
pub use stream::{PlayerConfig, RtPlayer, StreamHandle, StreamMetrics};

/// One-call convenience path: constant ratio, constant transpose, offline.
///
/// Everything it does is available separately; it exists so a caller that only
/// wants "make this 1.5x longer" does not have to assemble a document by hand.
pub fn stretch_constant(
    source: &AudioBuffer,
    sample_rate: u32,
    alpha: f64,
    semitones: f64,
    mode: EngineMode,
) -> Result<AudioBuffer, RenderError> {
    let id = SourceIdentity::of(source, sample_rate);
    let doc = EditDocument::constant(id, alpha, semitones, mode);
    let analysis = if mode == EngineMode::Auto {
        Some(analysis::analyze(
            source,
            sample_rate,
            "inline",
            &analysis::AnalysisSettings::default(),
        ))
    } else {
        None
    };
    let plan = compile(&doc, analysis.as_ref(), &CompileOptions::default())?;
    let (out, _) = render_offline(&plan, source, 1024)?;
    Ok(out)
}
