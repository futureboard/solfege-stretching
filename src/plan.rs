//! Plan compilation: document + analysis -> one immutable render plan.
//!
//! Everything that can be rejected is rejected here, before a sample is
//! rendered: an infeasible transient constraint, an internal ratio outside the
//! engine's range, a formant request on an engine with no formant path, a group
//! whose members do not line up. The chosen mode is recorded in the plan so the
//! render repeats exactly (system-design.md sec.6).

use crate::analysis::{Analysis, ContentClass};
use crate::audio::Fnv1a128;
use crate::document::{EditDocument, EngineMode, FormantPolicy, QualityProfile};
use crate::engines::elastic::{ElasticEngine, ElasticPreset};
use crate::engines::{
    bypass::BypassEngine, pitch::PitchStage, soloist::SoloistEngine, tape::TapeEngine,
    texture::TextureEngine,
};
use crate::engines::{Capability, PrepareError, PreparedConfig, StretchEngine};
use crate::mapping::TimeMap;
use std::fmt;

/// Engine build version, part of the render cache key.
pub const ENGINE_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
pub enum PlanError {
    Document(String),
    /// Both controls are individually in range but their product is not.
    InternalRatio { alpha: f64, pitch: f64, internal: f64, min: f64, max: f64 },
    Prepare(PrepareError),
    /// Varispeed ties pitch to the map, so a separate transpose is a
    /// contradiction.
    TapePitchConflict { semitones: f64 },
    FormantUnsupported { mode: EngineMode },
    GroupMismatch(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Document(e) => write!(f, "{e}"),
            PlanError::InternalRatio { alpha, pitch, internal, min, max } => write!(
                f,
                "alpha {alpha:.3} with pitch x{pitch:.3} needs an internal stretch of \
                 {internal:.3}, outside the engine range {min}..{max}"
            ),
            PlanError::Prepare(e) => write!(f, "{e}"),
            PlanError::TapePitchConflict { semitones } => write!(
                f,
                "Varispeed derives pitch from the time map; it cannot also transpose by \
                 {semitones:+.2} semitones. Use an Elastic mode or Soloist for independent pitch."
            ),
            PlanError::FormantUnsupported { mode } => write!(
                f,
                "{} has no formant path; the Elastic modes and Soloist do",
                mode.label()
            ),
            PlanError::GroupMismatch(m) => write!(f, "group members do not line up: {m}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<PrepareError> for PlanError {
    fn from(e: PrepareError) -> Self {
        PlanError::Prepare(e)
    }
}

/// The compiled, immutable description of one render.
#[derive(Clone, Debug)]
pub struct RenderPlan {
    pub cfg: PreparedConfig,
    /// The concrete mode chosen, after `Auto` routing.
    pub mode: EngineMode,
    /// Why `Auto` chose it, for the UI. `None` when the user picked the mode.
    pub auto_reason: Option<String>,
    pub capability: Capability,
    pub render_key: String,
}

impl RenderPlan {
    pub fn output_frames(&self) -> u64 {
        self.cfg.output_frames()
    }
    pub fn alpha_range(&self) -> (f64, f64) {
        self.cfg.map.ratio_range()
    }
    pub fn internal_ratio_range(&self) -> (f64, f64) {
        self.cfg.internal_ratio_range()
    }
}

#[derive(Clone, Debug)]
pub struct CompileOptions {
    pub max_block: usize,
    /// Transient handling. `false` turns detection and locking off entirely,
    /// so "off" really is off rather than a very short protection.
    pub transient_protect: bool,
    /// Override the spectral window. `None` keeps the preset's choice.
    pub stft_size: Option<usize>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self { max_block: 4096, transient_protect: true, stft_size: None }
    }
}

pub fn capability_of(mode: EngineMode) -> Capability {
    match mode {
        EngineMode::Bypass => BypassEngine::CAPABILITY,
        EngineMode::Varispeed => TapeEngine::CAPABILITY,
        EngineMode::ElasticPro => Capability { name: "elastic pro", ..ElasticEngine::CAPABILITY },
        EngineMode::ElasticEfficient => {
            Capability { name: "elastic efficient", ..ElasticEngine::CAPABILITY }
        }
        EngineMode::Rhythmic => Capability { name: "rhythmic", ..ElasticEngine::CAPABILITY },
        EngineMode::Soloist => SoloistEngine::CAPABILITY,
        EngineMode::Texture => TextureEngine::CAPABILITY,
        EngineMode::Auto => ElasticEngine::CAPABILITY,
    }
}

fn route_auto(doc: &EditDocument, analysis: Option<&Analysis>, map: &TimeMap) -> (EngineMode, String) {
    if doc.is_identity() {
        return (EngineMode::Bypass, "no edits: identity render".to_string());
    }
    let Some(a) = analysis else {
        return (
            EngineMode::ElasticPro,
            "no analysis yet: Elastic Pro handles any material".to_string(),
        );
    };
    let mut mode = a.class.suggested_mode();
    let (lo, hi) = map.ratio_range();
    // Soloist is built for one voice at moderate ratios; far outside that
    // the spectral engine degrades more gracefully.
    if mode == EngineMode::Soloist && (hi > 3.0 || lo < 0.33) {
        mode = EngineMode::ElasticPro;
    }
    let reason = format!(
        "{} (percussivity {:.2}, tonality {:.2}, ratio {:.2}..{:.2})",
        a.class.label(),
        a.percussivity,
        a.tonality,
        lo,
        hi
    );
    (mode, reason)
}

/// Compile a document into a plan. Nothing is rendered and nothing is
/// allocated for audio here beyond the plan itself.
pub fn compile(
    doc: &EditDocument,
    analysis: Option<&Analysis>,
    opts: &CompileOptions,
) -> Result<RenderPlan, PlanError> {
    let map = doc.validate().map_err(|e| PlanError::Document(e.to_string()))?;
    let pitch = doc.pitch_multiplier();

    let (mode, auto_reason) = if doc.mode == EngineMode::Auto {
        let (m, why) = route_auto(doc, analysis, &map);
        (m, Some(why))
    } else {
        (doc.mode, None)
    };

    if mode == EngineMode::Varispeed && doc.pitch_semitones != 0.0 {
        return Err(PlanError::TapePitchConflict { semitones: doc.pitch_semitones });
    }

    let cap = capability_of(mode);
    if !doc.formant.is_identity() && !cap.formant_control {
        return Err(PlanError::FormantUnsupported { mode });
    }

    // Only the engines that transpose by stretch-then-resample see an
    // internal ratio of alpha*p; the Elastic modes and Soloist shift pitch
    // natively, so their stretch is just alpha.
    let (lo, hi) = map.ratio_range();
    let resampled = mode == EngineMode::Texture && pitch != 1.0;
    let (ilo, ihi) = if resampled { (lo * pitch, hi * pitch) } else { (lo, hi) };
    if map.source_frames().get() > 0 && (ilo < cap.min_ratio || ihi > cap.max_ratio) {
        let (alpha, internal) = if ilo < cap.min_ratio { (lo, ilo) } else { (hi, ihi) };
        return Err(PlanError::InternalRatio {
            alpha,
            pitch,
            internal,
            min: cap.min_ratio,
            max: cap.max_ratio,
        });
    }
    if cap.independent_pitch && !(0.25..=4.0).contains(&pitch) {
        return Err(PlanError::Prepare(PrepareError::UnsupportedPitch {
            requested: pitch,
            min: 0.25,
            max: 4.0,
        }));
    }

    let cfg = PreparedConfig {
        sample_rate: doc.source.sample_rate,
        channels: doc.source.channels,
        map,
        pitch,
        formant: doc.formant,
        quality: doc.quality,
        transient_protect: opts.transient_protect,
        max_block: opts.max_block,
        seed: doc.deterministic_seed,
        stft_size: opts.stft_size,
    };

    let render_key = render_key(doc, &mode, opts);
    Ok(RenderPlan { cfg, mode, auto_reason, capability: cap, render_key })
}

/// `render_key = hash(analysis_key, canonical_edit_document, engine_version,
/// quality_profile, output_format, deterministic_seed)`.
pub fn render_key(doc: &EditDocument, mode: &EngineMode, opts: &CompileOptions) -> String {
    let mut h = Fnv1a128::new();
    h.write_str(&doc.canonical_hash());
    h.write_u64(ENGINE_VERSION as u64);
    h.write_str(mode.label());
    h.write_str(doc.quality.label());
    h.write_u64(doc.deterministic_seed);
    h.write_u64(opts.transient_protect as u64);
    h.write_u64(opts.max_block as u64);
    h.write_u64(opts.stft_size.unwrap_or(0) as u64);
    h.finish_hex()
}

/// Build and prepare the engine for a plan.
///
/// The Elastic modes and Soloist transpose natively. Texture still transposes
/// by stretch-then-resample through [`PitchStage`]; when `p == 1` the stage is
/// left out entirely.
pub fn build_engine(plan: &RenderPlan) -> Result<Box<dyn StretchEngine>, PlanError> {
    let mut engine: Box<dyn StretchEngine> = match plan.mode {
        EngineMode::Bypass => Box::new(BypassEngine::new()),
        EngineMode::Varispeed => Box::new(TapeEngine::new()),
        EngineMode::ElasticPro | EngineMode::Auto => Box::new(ElasticEngine::new(ElasticPreset::Pro)),
        EngineMode::ElasticEfficient => Box::new(ElasticEngine::new(ElasticPreset::Efficient)),
        EngineMode::Rhythmic => Box::new(ElasticEngine::new(ElasticPreset::Rhythmic)),
        EngineMode::Soloist => Box::new(SoloistEngine::new()),
        EngineMode::Texture => {
            let inner: Box<dyn StretchEngine> = Box::new(TextureEngine::new());
            if plan.cfg.pitch == 1.0 {
                inner
            } else {
                Box::new(PitchStage::new(inner, plan.cfg.pitch))
            }
        }
    };
    engine.prepare(&plan.cfg)?;
    Ok(engine)
}

/// A group renders with one map and one set of protections. Members must share
/// rate, channel layout, origin and covered length; nothing is auto-aligned,
/// because silently changing a microphone delay is worse than a rejection
/// (system-design.md sec.9).
pub fn check_group(members: &[&EditDocument]) -> Result<(), PlanError> {
    let Some(first) = members.first() else {
        return Ok(());
    };
    for m in members.iter().skip(1) {
        if m.source.sample_rate != first.source.sample_rate {
            return Err(PlanError::GroupMismatch(format!(
                "sample rate {} vs {}",
                m.source.sample_rate, first.source.sample_rate
            )));
        }
        if m.source.channels != first.source.channels {
            return Err(PlanError::GroupMismatch(format!(
                "channel count {} vs {}",
                m.source.channels, first.source.channels
            )));
        }
        if m.source.frames != first.source.frames {
            return Err(PlanError::GroupMismatch(format!(
                "length {} vs {} frames",
                m.source.frames, first.source.frames
            )));
        }
        if m.anchors != first.anchors {
            return Err(PlanError::GroupMismatch("anchors differ".to_string()));
        }
    }
    Ok(())
}

/// Quality profile a ratio suggests. Advice for the UI, never an automatic
/// switch behind the user's back.
pub fn suggest_quality(alpha_max: f64) -> QualityProfile {
    if alpha_max > 2.0 {
        QualityProfile::Offline
    } else {
        QualityProfile::Realtime
    }
}

/// Formant policies an engine can honour, for building UI without guessing.
pub fn formant_options(mode: EngineMode) -> Vec<FormantPolicy> {
    if capability_of(mode).formant_control {
        vec![
            FormantPolicy::FollowPitch,
            FormantPolicy::Preserve,
            FormantPolicy::Shift(0.0),
        ]
    } else {
        vec![FormantPolicy::FollowPitch]
    }
}

/// Content classes that exist, for the UI legend.
pub const CONTENT_CLASSES: [ContentClass; 5] = [
    ContentClass::Silence,
    ContentClass::Percussive,
    ContentClass::MonophonicTonal,
    ContentClass::PolyphonicTonal,
    ContentClass::Mixed,
];
