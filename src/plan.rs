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
use crate::engines::{
    bypass::BypassEngine, percussive::PercussiveEngine, pitch::PitchStage, pv::PvEngine,
    tape::TapeEngine, texture::TextureEngine, wsola::WsolaEngine,
};
use crate::engines::{Capability, PrepareError, PreparedConfig, ProtectWindow, StretchEngine};
use crate::mapping::TimeMap;
use std::fmt;

/// Engine build version, part of the render cache key.
pub const ENGINE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub enum PlanError {
    Document(String),
    /// Protected attacks do not fit in the output span the map asks for.
    ConstraintConflict {
        segment: usize,
        protected_frames: u64,
        output_frames: u64,
        source_frames: u64,
    },
    /// Both controls are individually in range but their product is not.
    InternalRatio { alpha: f64, pitch: f64, internal: f64, min: f64, max: f64 },
    Prepare(PrepareError),
    /// Tape ties pitch to the map, so a separate transpose is a contradiction.
    TapePitchConflict { semitones: f64 },
    FormantUnsupported { mode: EngineMode },
    GroupMismatch(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Document(e) => write!(f, "{e}"),
            PlanError::ConstraintConflict {
                segment,
                protected_frames,
                output_frames,
                source_frames,
            } => write!(
                f,
                "segment {segment}: {protected_frames} protected frames do not fit in \
                 {output_frames} output frames (from {source_frames} source frames). \
                 Shorten the protection or accept less of it; the render will not \
                 silently drop a hit."
            ),
            PlanError::InternalRatio { alpha, pitch, internal, min, max } => write!(
                f,
                "alpha {alpha:.3} with pitch x{pitch:.3} needs an internal stretch of \
                 {internal:.3}, outside the engine range {min}..{max}"
            ),
            PlanError::Prepare(e) => write!(f, "{e}"),
            PlanError::TapePitchConflict { semitones } => write!(
                f,
                "Tape derives pitch from the time map; it cannot also transpose by \
                 {semitones:+.2} semitones. Use Monophonic or Polyphonic for independent pitch."
            ),
            PlanError::FormantUnsupported { mode } => write!(
                f,
                "{} has no formant path; only Polyphonic and Hybrid do",
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
    /// Attack length protected around each promoted onset, in seconds.
    pub protect_seconds: f64,
    /// Use detected onsets as protections even when the mode is not Percussive.
    pub protect_in_spectral_modes: bool,
    /// Override the spectral window. `None` keeps the engine's own choice.
    pub stft_size: Option<usize>,
    /// Override the low path's window; `Some(0)` turns the path off.
    pub low_stft_size: Option<usize>,
    pub low_gate: bool,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            max_block: 4096,
            protect_seconds: 0.012,
            protect_in_spectral_modes: true,
            stft_size: None,
            low_stft_size: None,
            // Off, and the reason is in `engines::pv`: handing the bass back
            // and forth between two independent phase evolutions is a
            // time-varying comb filter, and it measured far worse than either
            // side of the switch. Kept as a knob for a future design that
            // makes the two bands phase-coherent.
            low_gate: false,
        }
    }
}

/// Pick the spectral window from what the analysis found.
///
/// Window length is the one PV parameter with two opposed jobs (dsp.md sec.5):
/// long enough to resolve the partials of the lowest note present, short enough
/// to keep an attack where it was. Neither is a constant of the algorithm, they
/// are properties of the material, so the compiler reads them off the analysis
/// instead of pinning a number.
///
/// * **Bass** sets the floor. Partials sit `f0` apart, which is `f0*N/rate`
///   bins, and peak locking needs a few bins between them to tell one partial
///   from the next - so `N >= BINS_PER_PARTIAL * rate / f0`.
/// * **Percussivity** sets the ceiling. Material with real attacks in it gets
///   the shorter window, because a smeared hit is more obvious than a slightly
///   rough bass note.
///
/// This is adaptive *per plan*, not multi-resolution per band: one window for
/// the whole render. True multi-resolution is the backlog item dsp.md sec.2
/// lists, and calling this that would be an overclaim.
const BINS_PER_PARTIAL: f64 = 3.5;
/// Bins the main window must place across the crossover transition so the two
/// bands draw the same curve and sum back to unity.
const CROSSOVER_BINS: f64 = 5.0;

pub fn choose_stft_size(
    analysis: Option<&Analysis>,
    sample_rate: u32,
    quality: QualityProfile,
    hybrid: bool,
    low_band: bool,
) -> Option<usize> {
    let a = analysis?;
    let base: usize = match quality {
        QualityProfile::Offline => 2048,
        QualityProfile::Realtime => 1024,
    };
    // Lowest fundamental worth designing for: the 10th percentile of confident
    // detections, so one bad octave estimate cannot drag the window out.
    let mut lows: Vec<f64> = a
        .notes
        .iter()
        .filter(|n| n.confidence > 0.6 && n.median_hz > 20.0)
        .map(|n| n.median_hz)
        .collect();
    if lows.is_empty() {
        return None;
    }
    lows.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let f0_low = lows[lows.len() / 10];

    // With the specialised low path running, the bass is no longer this
    // window's problem - but the crossover is. The two bands only add back to
    // unity if both can draw the same transition, and a window that spans the
    // 60 Hz transition in two bins draws it as a step: on a real master that
    // showed up as +3.01 dBFS of peak against +1.32 for a window one size up,
    // purely from ripple in the overlap. So the floor comes from the transition
    // width, not from the lowest note.
    let need = if low_band {
        let transition = crate::engines::pv::CROSSOVER_HI_HZ - crate::engines::pv::CROSSOVER_LO_HZ;
        (CROSSOVER_BINS * sample_rate as f64 / transition).ceil() as usize
    } else {
        (BINS_PER_PARTIAL * sample_rate as f64 / f0_low).ceil() as usize
    };
    let mut n = need.next_power_of_two().max(base);
    // Attacks win over bass when there are attacks.
    if a.percussivity > 0.10 {
        n = n.min(base);
    }
    if hybrid && !low_band {
        // The separation needs more resolution than the synthesis; see the
        // window note in `engines::pv`. Not when the low path is running,
        // though: HPSS is then only asked about frequencies above the
        // crossover, where partials are already far enough apart, and doubling
        // here would both cost every attack and push the main window up to the
        // low path's own size - which would disable it.
        n *= 2;
    }
    Some(n.clamp(512, 8192))
}

/// Should the specialised low path run, and with what window?
///
/// `Some(0)` disables it. The path resolves a bass fundamental far better than
/// the main window can - a 82.5 Hz tone measured 0.23 cents of wobble without
/// it and 0.05 with - but its window is long enough to smear a low-frequency
/// attack, and the two cannot be crossfaded (see `engines::pv`). So the
/// decision is made once, from the material, and never switched mid-render.
///
/// Two things turn it off: no bass to help, and low-frequency transients to
/// ruin. On a pure drum fixture the attack rise went from 2.36 ms to 9.09 ms
/// with the path on, while on bass with drums over it the low end got *smoother*
/// (0.28 to 0.14 dB rms of envelope roughness). Percussivity separates those.
pub fn choose_low_stft_size(
    analysis: Option<&Analysis>,
    sample_rate: u32,
    quality: QualityProfile,
) -> Option<usize> {
    let a = analysis?;
    let mut lows: Vec<f64> = a
        .notes
        .iter()
        .filter(|n| n.confidence > 0.6 && n.median_hz > 20.0)
        .map(|n| n.median_hz)
        .collect();
    if lows.is_empty() {
        return Some(0);
    }
    lows.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let f0_low = lows[lows.len() / 10];
    if f0_low > crate::engines::pv::CROSSOVER_HI_HZ {
        // Nothing lives below the crossover; the path would only cost CPU.
        return Some(0);
    }
    if a.percussivity > 0.10 {
        return Some(0);
    }
    let n = match quality {
        QualityProfile::Offline => 8192,
        QualityProfile::Realtime => 4096,
    };
    Some(if sample_rate > 60_000 { n * 2 } else { n })
}

/// Merge overlapping protection windows. dsp.md sec.6 requires this *before*
/// the feasibility arithmetic, otherwise overlapping attacks are counted twice.
pub fn merge_protections(mut windows: Vec<ProtectWindow>) -> Vec<ProtectWindow> {
    windows.retain(|w| !w.is_empty());
    windows.sort_by_key(|w| w.start);
    let mut out: Vec<ProtectWindow> = Vec::with_capacity(windows.len());
    for w in windows {
        match out.last_mut() {
            Some(last) if w.start <= last.end => {
                last.end = last.end.max(w.end);
            }
            _ => out.push(w),
        }
    }
    out
}

/// `alpha_sustain = (L_out - P) / (L_in - P)`, valid only while both sides stay
/// positive (dsp.md sec.6).
pub fn sustain_ratio(l_in: u64, l_out: u64, protected: u64) -> Option<f64> {
    if l_in <= protected || l_out <= protected {
        return None;
    }
    Some((l_out - protected) as f64 / (l_in - protected) as f64)
}

fn check_protections(map: &TimeMap, protections: &[ProtectWindow]) -> Result<(), PlanError> {
    for i in 0..map.segment_count() {
        let a = map.anchors()[i];
        let b = map.anchors()[i + 1];
        let l_in = b.source_frame - a.source_frame;
        let l_out = b.output_frame - a.output_frame;
        let protected: u64 = protections
            .iter()
            .map(|p| {
                let s = p.start.max(a.source_frame);
                let e = p.end.min(b.source_frame);
                e.saturating_sub(s)
            })
            .sum();
        if protected == 0 {
            continue;
        }
        if sustain_ratio(l_in, l_out, protected).is_none() {
            return Err(PlanError::ConstraintConflict {
                segment: i,
                protected_frames: protected,
                output_frames: l_out,
                source_frames: l_in,
            });
        }
    }
    Ok(())
}

fn capability_of(mode: EngineMode) -> Capability {
    match mode {
        EngineMode::Bypass => BypassEngine::CAPABILITY,
        EngineMode::Tape => TapeEngine::CAPABILITY,
        EngineMode::Percussive => PercussiveEngine::CAPABILITY,
        EngineMode::Monophonic => WsolaEngine::CAPABILITY,
        EngineMode::Polyphonic => PvEngine::CAPABILITY,
        EngineMode::Hybrid => Capability { name: "hybrid", ..PvEngine::CAPABILITY },
        EngineMode::Texture => TextureEngine::CAPABILITY,
        EngineMode::Auto => WsolaEngine::CAPABILITY,
    }
}

fn route_auto(doc: &EditDocument, analysis: Option<&Analysis>, map: &TimeMap) -> (EngineMode, String) {
    if doc.is_identity() {
        return (EngineMode::Bypass, "no edits: identity render".to_string());
    }
    let Some(a) = analysis else {
        return (
            EngineMode::Monophonic,
            "no analysis available: WSOLA is the safe baseline".to_string(),
        );
    };
    let mode = a.class.suggested_mode();
    let (lo, hi) = map.ratio_range();
    // A large ratio on percussive material still wants slicing, but a very
    // large one on tonal material is better served spectrally.
    let mode = if mode == EngineMode::Monophonic && (hi > 2.0 || lo < 0.5) {
        EngineMode::Polyphonic
    } else {
        mode
    };
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

    if mode == EngineMode::Tape && doc.pitch_semitones != 0.0 {
        return Err(PlanError::TapePitchConflict { semitones: doc.pitch_semitones });
    }

    let cap = capability_of(mode);
    if !doc.formant.is_identity() && !cap.formant_control {
        return Err(PlanError::FormantUnsupported { mode });
    }

    // Protections: promoted onsets, merged, clipped to the source.
    let want_protect = matches!(mode, EngineMode::Percussive)
        || (opts.protect_in_spectral_modes
            && matches!(mode, EngineMode::Polyphonic | EngineMode::Hybrid | EngineMode::Monophonic));
    let protections = if want_protect {
        let len = (doc.source.sample_rate as f64 * opts.protect_seconds).round() as u64;
        let windows: Vec<ProtectWindow> = analysis
            .map(|a| {
                a.onsets
                    .iter()
                    .map(|o| ProtectWindow {
                        start: o.frame,
                        end: (o.frame + len).min(doc.source.frames),
                    })
                    .collect()
            })
            .unwrap_or_default();
        merge_protections(windows)
    } else {
        Vec::new()
    };

    if mode == EngineMode::Percussive {
        check_protections(&map, &protections)?;
    }

    // The internal ratio is what the engine actually has to realise.
    let (lo, hi) = map.ratio_range();
    let (ilo, ihi) = (lo * pitch, hi * pitch);
    if ilo < cap.min_ratio || ihi > cap.max_ratio {
        let (alpha, internal) =
            if ilo < cap.min_ratio { (lo, ilo) } else { (hi, ihi) };
        return Err(PlanError::InternalRatio {
            alpha,
            pitch,
            internal,
            min: cap.min_ratio,
            max: cap.max_ratio,
        });
    }

    // The low path is decided first, because whether it runs changes what the
    // main window has to cover.
    let low_stft_size = opts.low_stft_size.or_else(|| {
        if matches!(mode, EngineMode::Polyphonic | EngineMode::Hybrid) {
            choose_low_stft_size(analysis, doc.source.sample_rate, doc.quality)
        } else {
            None
        }
    });
    let low_running = low_stft_size.map(|v| v > 0).unwrap_or(false);

    // An explicit override wins; otherwise let the material choose.
    let stft_size = opts.stft_size.or_else(|| {
        if matches!(mode, EngineMode::Polyphonic | EngineMode::Hybrid) {
            choose_stft_size(
                analysis,
                doc.source.sample_rate,
                doc.quality,
                mode == EngineMode::Hybrid,
                low_running,
            )
        } else {
            None
        }
    });

    let cfg = PreparedConfig {
        sample_rate: doc.source.sample_rate,
        channels: doc.source.channels,
        map,
        pitch,
        formant: doc.formant,
        quality: doc.quality,
        protections,
        protect_frames: (doc.source.sample_rate as f64 * opts.protect_seconds).round() as u64,
        transient_protect: opts.protect_seconds > 0.0,
        max_block: opts.max_block,
        seed: doc.deterministic_seed,
        stft_size,
        low_stft_size,
        low_gate: opts.low_gate,
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
    h.write_f64(opts.protect_seconds);
    h.write_u64(opts.max_block as u64);
    h.write_u64(opts.stft_size.unwrap_or(0) as u64);
    h.write_u64(opts.low_stft_size.map(|v| v as u64 + 1).unwrap_or(0));
    h.write_u64(opts.low_gate as u64);
    h.finish_hex()
}

/// Build and prepare the engine chain for a plan.
///
/// Transposition is a separate stage wrapped around the stretch engine, so no
/// engine has to know about both clocks. When `p == 1` the stage is left out
/// entirely, which is what keeps Bypass sample-exact.
pub fn build_engine(plan: &RenderPlan) -> Result<Box<dyn StretchEngine>, PlanError> {
    let inner: Box<dyn StretchEngine> = match plan.mode {
        EngineMode::Bypass => Box::new(BypassEngine::new()),
        EngineMode::Tape => Box::new(TapeEngine::new()),
        EngineMode::Percussive => Box::new(PercussiveEngine::new()),
        EngineMode::Monophonic => Box::new(WsolaEngine::new()),
        EngineMode::Polyphonic => Box::new(PvEngine::new()),
        EngineMode::Hybrid => Box::new(PvEngine::hybrid()),
        EngineMode::Texture => Box::new(TextureEngine::new()),
        EngineMode::Auto => Box::new(WsolaEngine::new()),
    };

    let mut engine: Box<dyn StretchEngine> = if plan.cfg.pitch == 1.0 {
        inner
    } else {
        Box::new(PitchStage::new(inner, plan.cfg.pitch))
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
