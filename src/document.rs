//! The non-destructive edit document: anchors, note edits, mode and quality.
//!
//! Edits are stored relative to the source and are never applied to the source
//! samples. Detected notes and user note edits are separate objects with stable
//! ids, so re-running analysis does not overwrite user work
//! (system-design.md sec.4 and sec.10).

use crate::audio::{Fnv1a128, SourceIdentity};
use crate::mapping::{AnchorKind, MapError, TimeMap, WarpAnchor};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteId(pub u64);

/// How the spectral envelope follows a pitch change (dsp.md sec.8). The numbers
/// mean what this document says they mean; they are not borrowed from any other
/// vendor UI.
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "semitones")]
pub enum FormantPolicy {
    /// `f = 1`: keep the original envelope.
    Preserve,
    /// `f = p`: the envelope rides the pitch, as plain resampling does.
    FollowPitch,
    /// `f = 2^(st/12)`: an explicit envelope shift.
    Shift(f64),
}

impl FormantPolicy {
    /// The envelope multiplier `f` for a pitch multiplier `p`.
    pub fn factor(self, p: f64) -> f64 {
        match self {
            FormantPolicy::Preserve => 1.0,
            FormantPolicy::FollowPitch => p,
            FormantPolicy::Shift(st) => 2f64.powf(st / 12.0),
        }
    }
    pub fn is_identity(self) -> bool {
        matches!(self, FormantPolicy::FollowPitch | FormantPolicy::Shift(0.0))
    }
}

impl Default for FormantPolicy {
    fn default() -> Self {
        FormantPolicy::FollowPitch
    }
}

/// The modes a user picks from, plus `Auto`, which is a routing policy the
/// user can always override rather than an engine of its own.
///
/// The lineup follows what every serious warping tool converged on - one
/// general-purpose spectral mode, a cheaper variant, a transient-first mode, a
/// monophonic mode, varispeed and an effect - but the implementations are this
/// crate's own (see `engines::elastic` and `engines::soloist`).
///
/// Documents written before the rework used other names; they still load,
/// through the serde aliases, onto the nearest new mode.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineMode {
    /// Identity copy. Only valid for an edit that changes nothing.
    Bypass,
    /// General purpose, highest quality: mixes, keys, pads, vocals with
    /// backing. Phase-gradient phase vocoder, 85 ms window, 8x overlap.
    #[serde(alias = "polyphonic", alias = "hybrid")]
    ElasticPro,
    /// The same engine with a 43 ms window and 4x overlap: a quarter of the
    /// work, slightly less bass definition.
    ElasticEfficient,
    /// Transients first: drums, percussive loops, rhythm parts. Short window,
    /// every detected hit played at unity rate.
    #[serde(alias = "percussive")]
    Rhythmic,
    /// One voice or one instrument line. Pitch-synchronous overlap-add:
    /// no phasiness, and formants stay put under transposition.
    #[serde(alias = "monophonic")]
    Soloist,
    /// Tape: speed and pitch move together. No independent transpose.
    #[serde(alias = "tape")]
    Varispeed,
    /// Granular effect. Changes the sound on purpose.
    Texture,
    Auto,
}

impl EngineMode {
    pub fn label(self) -> &'static str {
        match self {
            EngineMode::Bypass => "bypass",
            EngineMode::ElasticPro => "elastic-pro",
            EngineMode::ElasticEfficient => "elastic-efficient",
            EngineMode::Rhythmic => "rhythmic",
            EngineMode::Soloist => "soloist",
            EngineMode::Varispeed => "varispeed",
            EngineMode::Texture => "texture",
            EngineMode::Auto => "auto",
        }
    }
    /// Human name for menus.
    pub fn title(self) -> &'static str {
        match self {
            EngineMode::Bypass => "Bypass",
            EngineMode::ElasticPro => "Elastic Pro",
            EngineMode::ElasticEfficient => "Elastic Efficient",
            EngineMode::Rhythmic => "Rhythmic",
            EngineMode::Soloist => "Soloist",
            EngineMode::Varispeed => "Varispeed",
            EngineMode::Texture => "Texture (FX)",
            EngineMode::Auto => "Auto",
        }
    }
    /// One line on what the mode is for.
    pub fn hint(self) -> &'static str {
        match self {
            EngineMode::Bypass => "no processing",
            EngineMode::ElasticPro => "anything: full mixes, keys, pads, vocals with backing",
            EngineMode::ElasticEfficient => "same as Pro at a quarter of the CPU",
            EngineMode::Rhythmic => "drums, loops, anything where the hit matters most",
            EngineMode::Soloist => "one voice or instrument: vocals, bass, lead lines",
            EngineMode::Varispeed => "tape: pitch follows speed",
            EngineMode::Texture => "granular effect",
            EngineMode::Auto => "pick from the analysis",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "bypass" => EngineMode::Bypass,
            "elastic-pro" | "elastic" | "pro" | "polyphonic" | "pv" | "hybrid" => EngineMode::ElasticPro,
            "elastic-efficient" | "efficient" => EngineMode::ElasticEfficient,
            "rhythmic" | "percussive" | "slicing" | "drums" => EngineMode::Rhythmic,
            "soloist" | "monophonic" | "solo" | "wsola" => EngineMode::Soloist,
            "varispeed" | "tape" | "speed" => EngineMode::Varispeed,
            "texture" => EngineMode::Texture,
            "auto" => EngineMode::Auto,
            _ => return None,
        })
    }
    pub const ALL: [EngineMode; 8] = [
        EngineMode::Auto,
        EngineMode::ElasticPro,
        EngineMode::ElasticEfficient,
        EngineMode::Rhythmic,
        EngineMode::Soloist,
        EngineMode::Varispeed,
        EngineMode::Texture,
        EngineMode::Bypass,
    ];
    /// Can this mode transpose independently of time?
    pub fn independent_pitch(self) -> bool {
        !matches!(self, EngineMode::Varispeed | EngineMode::Bypass)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityProfile {
    /// Smaller hops and wider search: for rendering, not for a callback.
    Offline,
    /// Bounded work per block.
    Realtime,
}

impl QualityProfile {
    pub fn label(self) -> &'static str {
        match self {
            QualityProfile::Offline => "offline",
            QualityProfile::Realtime => "realtime",
        }
    }
}

/// A user edit on one detected note. Ranges are in **source time**; project
/// pitch automation, when it arrives, will be in output time and must say so.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct NoteEdit {
    pub id: NoteId,
    pub source_start: u64,
    /// Exclusive.
    pub source_end: u64,
    pub pitch_shift_cents: f64,
    pub drift_start_cents: f64,
    pub drift_end_cents: f64,
    pub vibrato_scale: f64,
    pub gain_db: f64,
    pub formant: FormantPolicy,
}

impl NoteEdit {
    pub fn identity(id: NoteId, source_start: u64, source_end: u64) -> Self {
        Self {
            id,
            source_start,
            source_end,
            pitch_shift_cents: 0.0,
            drift_start_cents: 0.0,
            drift_end_cents: 0.0,
            vibrato_scale: 1.0,
            gain_db: 0.0,
            formant: FormantPolicy::Preserve,
        }
    }
    pub fn is_identity(&self) -> bool {
        self.pitch_shift_cents == 0.0
            && self.drift_start_cents == 0.0
            && self.drift_end_cents == 0.0
            && self.vibrato_scale == 1.0
            && self.gain_db == 0.0
    }
}

/// A group of tracks that share one time map and one set of transient
/// decisions. Membership is checked by the plan compiler, never auto-aligned.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct GroupIdentity {
    pub id: String,
    pub members: Vec<String>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct EditDocument {
    pub schema_version: u32,
    pub source: SourceIdentity,
    pub anchors: Vec<WarpAnchor>,
    #[serde(default)]
    pub notes: Vec<NoteEdit>,
    pub mode: EngineMode,
    pub quality: QualityProfile,
    pub pitch_semitones: f64,
    pub formant: FormantPolicy,
    #[serde(default)]
    pub group: Option<GroupIdentity>,
    /// FX engines are seeded so a render can be repeated exactly.
    #[serde(default)]
    pub deterministic_seed: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DocumentError {
    SchemaVersion { found: u32, supported: u32 },
    SourceMismatch { expected: String, found: String },
    Map(MapError),
    NonFinite(&'static str),
    NoteOutOfRange { id: NoteId, source_end: u64, frames: u64 },
    NoteOverlap { a: NoteId, b: NoteId },
    NoteEmpty(NoteId),
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::SchemaVersion { found, supported } => {
                write!(f, "document schema {found} is not supported (this build reads {supported})")
            }
            DocumentError::SourceMismatch { expected, found } => {
                write!(f, "document belongs to source {expected}, not {found}")
            }
            DocumentError::Map(e) => write!(f, "invalid time map: {e}"),
            DocumentError::NonFinite(w) => write!(f, "{w} must be finite"),
            DocumentError::NoteOutOfRange { id, source_end, frames } => {
                write!(f, "note {id:?} ends at {source_end}, past the source length {frames}")
            }
            DocumentError::NoteOverlap { a, b } => write!(f, "notes {a:?} and {b:?} overlap"),
            DocumentError::NoteEmpty(id) => write!(f, "note {id:?} is empty"),
        }
    }
}

impl std::error::Error for DocumentError {}

impl From<MapError> for DocumentError {
    fn from(e: MapError) -> Self {
        DocumentError::Map(e)
    }
}

impl EditDocument {
    /// A document that renders the source unchanged.
    pub fn identity(source: SourceIdentity) -> Self {
        let n = source.frames;
        let anchors = if n == 0 {
            Vec::new()
        } else {
            vec![WarpAnchor::endpoint(0, 0), WarpAnchor::endpoint(n, n)]
        };
        Self {
            schema_version: SCHEMA_VERSION,
            source,
            anchors,
            notes: Vec::new(),
            mode: EngineMode::Bypass,
            quality: QualityProfile::Offline,
            pitch_semitones: 0.0,
            formant: FormantPolicy::FollowPitch,
            group: None,
            deterministic_seed: 0,
        }
    }

    /// Constant time ratio and constant transpose.
    pub fn constant(source: SourceIdentity, alpha: f64, semitones: f64, mode: EngineMode) -> Self {
        let mut d = Self::identity(source);
        let n = d.source.frames;
        if n > 0 {
            let out = ((n as f64) * alpha).round().max(1.0) as u64;
            d.anchors = vec![WarpAnchor::endpoint(0, 0), WarpAnchor::endpoint(n, out)];
        }
        d.pitch_semitones = semitones;
        d.mode = mode;
        d
    }

    /// `p = 2^(semitones/12)`.
    pub fn pitch_multiplier(&self) -> f64 {
        2f64.powf(self.pitch_semitones / 12.0)
    }

    pub fn time_map(&self) -> Result<TimeMap, MapError> {
        TimeMap::new(self.anchors.clone())
    }

    pub fn set_output_frames(&mut self, out: u64) -> Result<(), MapError> {
        let n = self.source.frames;
        if n == 0 {
            self.anchors.clear();
            return Ok(());
        }
        let old = TimeMap::new(self.anchors.clone())?;
        let old_out = old.output_frames().get() as f64;
        let scale = out as f64 / old_out;
        let mut anchors: Vec<WarpAnchor> = self
            .anchors
            .iter()
            .map(|a| WarpAnchor::new(a.source_frame, (a.output_frame as f64 * scale).round() as u64, a.kind))
            .collect();
        if let Some(last) = anchors.last_mut() {
            last.output_frame = out;
            last.kind = AnchorKind::Endpoint;
        }
        TimeMap::new(anchors.clone())?;
        self.anchors = anchors;
        Ok(())
    }

    /// Full validation, run before a render is compiled. Nothing is repaired
    /// silently; the caller gets a typed error instead.
    pub fn validate(&self) -> Result<TimeMap, DocumentError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(DocumentError::SchemaVersion {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if !self.pitch_semitones.is_finite() {
            return Err(DocumentError::NonFinite("pitch_semitones"));
        }
        if let FormantPolicy::Shift(st) = self.formant {
            if !st.is_finite() {
                return Err(DocumentError::NonFinite("formant shift"));
            }
        }
        let map = TimeMap::new(self.anchors.clone())?;
        if self.source.frames == 0 && map.output_frames().get() != 0 {
            return Err(DocumentError::Map(MapError::EmptySourceNonEmptyOutput));
        }
        if self.source.frames > 0 && map.source_frames().get() != self.source.frames {
            return Err(DocumentError::Map(MapError::SourceNotStrictlyIncreasing {
                index: self.anchors.len().saturating_sub(1),
                previous: self.source.frames,
                found: map.source_frames().get(),
            }));
        }

        let mut notes: Vec<&NoteEdit> = self.notes.iter().collect();
        notes.sort_by_key(|n| n.source_start);
        for w in notes.windows(2) {
            if w[1].source_start < w[0].source_end {
                return Err(DocumentError::NoteOverlap { a: w[0].id, b: w[1].id });
            }
        }
        for n in &self.notes {
            if n.source_end <= n.source_start {
                return Err(DocumentError::NoteEmpty(n.id));
            }
            if n.source_end > self.source.frames {
                return Err(DocumentError::NoteOutOfRange {
                    id: n.id,
                    source_end: n.source_end,
                    frames: self.source.frames,
                });
            }
            for (name, v) in [
                ("pitch_shift_cents", n.pitch_shift_cents),
                ("drift_start_cents", n.drift_start_cents),
                ("drift_end_cents", n.drift_end_cents),
                ("vibrato_scale", n.vibrato_scale),
                ("gain_db", n.gain_db),
            ] {
                if !v.is_finite() {
                    return Err(DocumentError::NonFinite(match name {
                        "pitch_shift_cents" => "note pitch_shift_cents",
                        "drift_start_cents" => "note drift_start_cents",
                        "drift_end_cents" => "note drift_end_cents",
                        "vibrato_scale" => "note vibrato_scale",
                        _ => "note gain_db",
                    }));
                }
            }
        }
        Ok(map)
    }

    /// True when nothing at all is asked of the engine: identity map, unit
    /// pitch, identity formants and no active note edit. Bypass must be
    /// sample-exact in this case (validation.md sec.4).
    pub fn is_identity(&self) -> bool {
        let map_identity = match TimeMap::new(self.anchors.clone()) {
            Ok(m) => m.is_identity(),
            Err(_) => false,
        };
        map_identity
            && self.pitch_semitones == 0.0
            && self.formant.is_identity()
            && self.notes.iter().all(|n| n.is_identity())
    }

    /// Canonical bytes for cache keys: field order fixed here, not by the JSON
    /// serializer, so the key does not move when the schema gains a field.
    pub fn canonical_hash(&self) -> String {
        let mut h = Fnv1a128::new();
        h.write_u64(self.schema_version as u64);
        h.write_str(&self.source.id);
        h.write_u64(self.source.sample_rate as u64);
        h.write_u64(self.source.channels as u64);
        h.write_u64(self.source.frames);
        for a in &self.anchors {
            h.write_u64(a.source_frame);
            h.write_u64(a.output_frame);
            h.write_u64(a.kind as u64);
        }
        for n in &self.notes {
            h.write_u64(n.id.0);
            h.write_u64(n.source_start);
            h.write_u64(n.source_end);
            h.write_f64(n.pitch_shift_cents);
            h.write_f64(n.drift_start_cents);
            h.write_f64(n.drift_end_cents);
            h.write_f64(n.vibrato_scale);
            h.write_f64(n.gain_db);
            h.write_f64(n.formant.factor(1.0));
        }
        h.write_str(self.mode.label());
        h.write_str(self.quality.label());
        h.write_f64(self.pitch_semitones);
        h.write_f64(self.formant.factor(2.0));
        h.write_u64(self.deterministic_seed);
        h.finish_hex()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("document is serializable")
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// A note the analyzer found. Kept apart from [`NoteEdit`] so re-analysis never
/// rewrites user edits; relinking is an explicit review step.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct DetectedNote {
    pub id: NoteId,
    pub source_start: u64,
    pub source_end: u64,
    pub median_hz: f64,
    pub confidence: f64,
    pub voiced_ratio: f64,
}

impl DetectedNote {
    pub fn midi(&self) -> f64 {
        69.0 + 12.0 * (self.median_hz / 440.0).log2()
    }
}
