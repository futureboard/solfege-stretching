//! Monotonic piecewise-linear time map, its inverse, and anchor validation.
//!
//! Contract (system-design.md sec.5):
//!
//! * anchors are **sample boundaries**, so source `N` is the exclusive end and
//!   output `M` is the output frame count,
//! * `s_{i+1} > s_i` and `t_{i+1} > t_i` strictly; crossed or duplicated
//!   anchors are errors, never silently repaired,
//! * `(0,0)` and `(N,M)` must both be present for a local clip render,
//! * `N = 0` yields an empty output and no segment at all,
//! * interpolation is `f64`, endpoints stay locked integers so no per-block
//!   rounding can accumulate,
//! * nothing is extrapolated outside the source.

use crate::audio::{OutputFrame, SourceFrame};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    /// `(0,0)` or `(N,M)`.
    Endpoint,
    /// Placed or moved by the user; a hard timing requirement.
    User,
    /// A detected event the user promoted to a hard requirement.
    PromotedAnalysis,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WarpAnchor {
    pub source_frame: u64,
    pub output_frame: u64,
    pub kind: AnchorKind,
}

impl WarpAnchor {
    pub fn new(source_frame: u64, output_frame: u64, kind: AnchorKind) -> Self {
        Self { source_frame, output_frame, kind }
    }
    pub fn endpoint(source_frame: u64, output_frame: u64) -> Self {
        Self::new(source_frame, output_frame, AnchorKind::Endpoint)
    }
    pub fn user(source_frame: u64, output_frame: u64) -> Self {
        Self::new(source_frame, output_frame, AnchorKind::User)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MapError {
    TooFewAnchors,
    MissingOrigin,
    SourceNotStrictlyIncreasing { index: usize, previous: u64, found: u64 },
    OutputNotStrictlyIncreasing { index: usize, previous: u64, found: u64 },
    NonEmptySourceEmptyOutput,
    EmptySourceNonEmptyOutput,
    RatioOutOfRange { index: usize, ratio: f64, min: f64, max: f64 },
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::TooFewAnchors => write!(f, "a time map needs at least two anchors"),
            MapError::MissingOrigin => write!(f, "the first anchor must be (0,0)"),
            MapError::SourceNotStrictlyIncreasing { index, previous, found } => write!(
                f,
                "anchor {index}: source frame {found} is not greater than previous {previous}"
            ),
            MapError::OutputNotStrictlyIncreasing { index, previous, found } => write!(
                f,
                "anchor {index}: output frame {found} is not greater than previous {previous}"
            ),
            MapError::NonEmptySourceEmptyOutput => {
                write!(f, "output length 0 from a non-empty source is a delete, not a stretch")
            }
            MapError::EmptySourceNonEmptyOutput => {
                write!(f, "an empty source cannot produce output frames")
            }
            MapError::RatioOutOfRange { index, ratio, min, max } => write!(
                f,
                "segment {index}: local ratio {ratio:.6} outside supported range {min}..{max}"
            ),
        }
    }
}

impl std::error::Error for MapError {}

/// A validated, monotonic, piecewise-linear map from source frames to output
/// frames. Construction is the only way to get one, so every consumer can rely
/// on the invariants above.
#[derive(Clone, Debug, PartialEq)]
pub struct TimeMap {
    anchors: Vec<WarpAnchor>,
}

impl TimeMap {
    /// The empty map: zero source frames, zero output frames, no segments.
    pub fn empty() -> Self {
        Self { anchors: Vec::new() }
    }

    pub fn new(mut anchors: Vec<WarpAnchor>) -> Result<Self, MapError> {
        if anchors.is_empty() {
            return Ok(Self::empty());
        }
        anchors.sort_by_key(|a| (a.source_frame, a.output_frame));
        if anchors.len() < 2 {
            return Err(MapError::TooFewAnchors);
        }
        if anchors[0].source_frame != 0 || anchors[0].output_frame != 0 {
            return Err(MapError::MissingOrigin);
        }
        for i in 1..anchors.len() {
            let (p, c) = (anchors[i - 1], anchors[i]);
            if c.source_frame <= p.source_frame {
                return Err(MapError::SourceNotStrictlyIncreasing {
                    index: i,
                    previous: p.source_frame,
                    found: c.source_frame,
                });
            }
            if c.output_frame <= p.output_frame {
                return Err(MapError::OutputNotStrictlyIncreasing {
                    index: i,
                    previous: p.output_frame,
                    found: c.output_frame,
                });
            }
        }
        Ok(Self { anchors })
    }

    /// Constant-ratio map over `source_frames`. `alpha > 1` lengthens.
    pub fn constant(source_frames: u64, alpha: f64) -> Result<Self, MapError> {
        if source_frames == 0 {
            return Ok(Self::empty());
        }
        if !alpha.is_finite() || alpha <= 0.0 {
            return Err(MapError::RatioOutOfRange {
                index: 0,
                ratio: alpha,
                min: f64::MIN_POSITIVE,
                max: f64::INFINITY,
            });
        }
        let out = ((source_frames as f64) * alpha).round() as u64;
        if out == 0 {
            return Err(MapError::NonEmptySourceEmptyOutput);
        }
        Self::new(vec![
            WarpAnchor::endpoint(0, 0),
            WarpAnchor::endpoint(source_frames, out),
        ])
    }

    pub fn anchors(&self) -> &[WarpAnchor] {
        &self.anchors
    }

    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }

    pub fn source_frames(&self) -> SourceFrame {
        SourceFrame(self.anchors.last().map(|a| a.source_frame).unwrap_or(0))
    }

    pub fn output_frames(&self) -> OutputFrame {
        OutputFrame(self.anchors.last().map(|a| a.output_frame).unwrap_or(0))
    }

    pub fn segment_count(&self) -> usize {
        self.anchors.len().saturating_sub(1)
    }

    /// Duration ratio of segment `i` (`dt/ds`).
    pub fn segment_ratio(&self, i: usize) -> f64 {
        let a = self.anchors[i];
        let b = self.anchors[i + 1];
        (b.output_frame - a.output_frame) as f64 / (b.source_frame - a.source_frame) as f64
    }

    pub fn ratio_range(&self) -> (f64, f64) {
        if self.is_empty() {
            return (1.0, 1.0);
        }
        let mut lo = f64::INFINITY;
        let mut hi = 0.0f64;
        for i in 0..self.segment_count() {
            let r = self.segment_ratio(i);
            lo = lo.min(r);
            hi = hi.max(r);
        }
        (lo, hi)
    }

    /// Reject a map whose local ratio leaves the range an engine declared.
    pub fn check_ratio_range(&self, min: f64, max: f64) -> Result<(), MapError> {
        for i in 0..self.segment_count() {
            let r = self.segment_ratio(i);
            if r < min || r > max {
                return Err(MapError::RatioOutOfRange { index: i, ratio: r, min, max });
            }
        }
        Ok(())
    }

    fn segment_for_source(&self, s: f64) -> usize {
        // Binary search on the source axis; clamped, never extrapolated.
        let n = self.segment_count();
        let mut lo = 0usize;
        let mut hi = n - 1;
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if (self.anchors[mid].source_frame as f64) <= s {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    fn segment_for_output(&self, t: f64) -> usize {
        let n = self.segment_count();
        let mut lo = 0usize;
        let mut hi = n - 1;
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if (self.anchors[mid].output_frame as f64) <= t {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// `t = W(s)`. Clamped to the source domain; never extrapolated.
    pub fn forward(&self, s: f64) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let n_in = self.source_frames().as_f64();
        let s = s.clamp(0.0, n_in);
        if s == n_in {
            return self.output_frames().as_f64();
        }
        let i = self.segment_for_source(s);
        let a = self.anchors[i];
        let b = self.anchors[i + 1];
        let f = (s - a.source_frame as f64) / ((b.source_frame - a.source_frame) as f64);
        a.output_frame as f64 + f * ((b.output_frame - a.output_frame) as f64)
    }

    /// `s = W^-1(t)`. Clamped to the output domain; never extrapolated.
    pub fn inverse(&self, t: f64) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let m_out = self.output_frames().as_f64();
        let t = t.clamp(0.0, m_out);
        if t == m_out {
            return self.source_frames().as_f64();
        }
        let i = self.segment_for_output(t);
        let a = self.anchors[i];
        let b = self.anchors[i + 1];
        let f = (t - a.output_frame as f64) / ((b.output_frame - a.output_frame) as f64);
        a.source_frame as f64 + f * ((b.source_frame - a.source_frame) as f64)
    }

    /// `alpha = dt/ds` at a source position (right-continuous at anchors).
    pub fn ratio_at_source(&self, s: f64) -> f64 {
        if self.is_empty() {
            return 1.0;
        }
        let s = s.clamp(0.0, self.source_frames().as_f64());
        self.segment_ratio(self.segment_for_source(s))
    }

    /// `alpha` at an output position: the local stretch the synthesis sees.
    pub fn ratio_at_output(&self, t: f64) -> f64 {
        if self.is_empty() {
            return 1.0;
        }
        let t = t.clamp(0.0, self.output_frames().as_f64());
        self.segment_ratio(self.segment_for_output(t))
    }

    /// `v = ds/dt`, the source read speed at an output position.
    pub fn speed_at_output(&self, t: f64) -> f64 {
        1.0 / self.ratio_at_output(t)
    }

    /// True when the map is the identity: same length, single unit segment.
    pub fn is_identity(&self) -> bool {
        if self.is_empty() {
            return true;
        }
        self.anchors.len() == 2
            && self.anchors[1].source_frame == self.anchors[1].output_frame
    }

    /// Insert or move an anchor, keeping the map valid. Returns the previous
    /// map untouched on error, so a rejected edit never half-applies.
    pub fn with_anchor(&self, anchor: WarpAnchor) -> Result<Self, MapError> {
        let mut anchors: Vec<WarpAnchor> = self
            .anchors
            .iter()
            .copied()
            .filter(|a| a.source_frame != anchor.source_frame)
            .collect();
        anchors.push(anchor);
        TimeMap::new(anchors)
    }

    pub fn without_anchor_at(&self, source_frame: u64) -> Result<Self, MapError> {
        let anchors: Vec<WarpAnchor> = self
            .anchors
            .iter()
            .copied()
            .filter(|a| !(a.source_frame == source_frame && a.kind != AnchorKind::Endpoint))
            .collect();
        TimeMap::new(anchors)
    }
}

/// Beat-domain helper: sources whose tempo changes must be mapped through beat
/// positions, never through one BPM for the whole file (dsp.md sec.1).
#[derive(Clone, Debug, PartialEq)]
pub struct BeatGrid {
    /// Ascending source frame positions of successive beats.
    pub beats: Vec<f64>,
}

impl BeatGrid {
    pub fn constant(bpm: f64, sample_rate: u32, source_frames: u64) -> Self {
        let spb = 60.0 / bpm * sample_rate as f64;
        let n = ((source_frames as f64) / spb).floor() as usize + 1;
        Self { beats: (0..=n).map(|i| i as f64 * spb).collect() }
    }

    /// Source frame -> beat position (fractional beats).
    pub fn beat_of(&self, frame: f64) -> f64 {
        if self.beats.len() < 2 {
            return 0.0;
        }
        match self.beats.binary_search_by(|b| b.partial_cmp(&frame).unwrap()) {
            Ok(i) => i as f64,
            Err(0) => 0.0,
            Err(i) if i >= self.beats.len() => (self.beats.len() - 1) as f64,
            Err(i) => {
                let (a, b) = (self.beats[i - 1], self.beats[i]);
                (i - 1) as f64 + (frame - a) / (b - a)
            }
        }
    }

    /// Build a map that places source beats onto a destination tempo.
    pub fn map_to_tempo(
        &self,
        dest_bpm: f64,
        sample_rate: u32,
        source_frames: u64,
    ) -> Result<TimeMap, MapError> {
        if source_frames == 0 {
            return Ok(TimeMap::empty());
        }
        let spb = 60.0 / dest_bpm * sample_rate as f64;
        let mut anchors = vec![WarpAnchor::endpoint(0, 0)];
        for (i, &src) in self.beats.iter().enumerate().skip(1) {
            if src >= source_frames as f64 {
                break;
            }
            let out = (i as f64 * spb).round() as u64;
            if out == 0 {
                continue;
            }
            anchors.push(WarpAnchor::new(
                src.round() as u64,
                out,
                AnchorKind::PromotedAnalysis,
            ));
        }
        let total_beats = self.beat_of(source_frames as f64);
        let out_end = (total_beats * spb).round().max(1.0) as u64;
        anchors.push(WarpAnchor::endpoint(source_frames, out_end));
        TimeMap::new(anchors)
    }
}
