//! Time-map contract (system-design.md sec.5, validation.md sec.4).

use solfege::document::{EditDocument, EngineMode};
use solfege::mapping::{AnchorKind, BeatGrid, MapError, TimeMap, WarpAnchor};
use solfege::SourceIdentity;

fn map(anchors: &[(u64, u64)]) -> Result<TimeMap, MapError> {
    TimeMap::new(
        anchors
            .iter()
            .map(|(s, t)| WarpAnchor::new(*s, *t, AnchorKind::User))
            .collect(),
    )
}

#[test]
fn empty_source_gives_empty_map() {
    let m = TimeMap::empty();
    assert!(m.is_empty());
    assert_eq!(m.source_frames().get(), 0);
    assert_eq!(m.output_frames().get(), 0);
    assert_eq!(m.segment_count(), 0);
    // No segment is created, and nothing panics on a lookup.
    assert_eq!(m.forward(10.0), 0.0);
    assert_eq!(m.inverse(10.0), 0.0);
    assert_eq!(TimeMap::constant(0, 2.0).unwrap(), TimeMap::empty());
}

#[test]
fn origin_and_endpoint_are_required() {
    assert_eq!(map(&[(10, 10), (100, 100)]).unwrap_err(), MapError::MissingOrigin);
    assert_eq!(map(&[(0, 0)]).unwrap_err(), MapError::TooFewAnchors);
}

#[test]
fn anchors_must_strictly_increase_on_both_axes() {
    assert!(matches!(
        map(&[(0, 0), (100, 100), (100, 200)]).unwrap_err(),
        MapError::SourceNotStrictlyIncreasing { .. }
    ));
    assert!(matches!(
        map(&[(0, 0), (100, 100), (200, 100)]).unwrap_err(),
        MapError::OutputNotStrictlyIncreasing { .. }
    ));
    // A crossed anchor is an error, never quietly reordered.
    assert!(matches!(
        map(&[(0, 0), (100, 500), (200, 300), (400, 600)]).unwrap_err(),
        MapError::OutputNotStrictlyIncreasing { .. }
    ));
}

#[test]
fn endpoints_are_exact_and_never_extrapolated() {
    let m = map(&[(0, 0), (240_000, 288_000), (480_000, 640_000)]).unwrap();
    assert_eq!(m.forward(0.0), 0.0);
    assert_eq!(m.forward(480_000.0), 640_000.0);
    assert_eq!(m.inverse(0.0), 0.0);
    assert_eq!(m.inverse(640_000.0), 480_000.0);
    // Outside the domain the map clamps rather than extending the last slope.
    assert_eq!(m.forward(999_999.0), 640_000.0);
    assert_eq!(m.forward(-5.0), 0.0);
    assert_eq!(m.inverse(999_999.0), 480_000.0);
}

#[test]
fn inverse_round_trip_is_within_half_a_frame() {
    let m = map(&[(0, 0), (240_000, 288_000), (480_000, 640_000)]).unwrap();
    let n = m.source_frames().as_f64();
    let mut worst: f64 = 0.0;
    for i in 0..=2000 {
        let s = n * i as f64 / 2000.0;
        let back = m.inverse(m.forward(s));
        worst = worst.max((back - s).abs());
    }
    assert!(worst <= 0.5, "worst round-trip error {worst} frames");
}

#[test]
fn forward_is_monotonic_everywhere() {
    let m = map(&[(0, 0), (1000, 3000), (5000, 3500), (9000, 12_000)]).unwrap();
    let mut prev = f64::NEG_INFINITY;
    for i in 0..=9000 {
        let t = m.forward(i as f64);
        assert!(t >= prev, "map went backwards at {i}: {t} < {prev}");
        prev = t;
    }
}

#[test]
fn segment_ratios_match_the_documented_example() {
    // docs/system-design.md sec.4: 1.2 then 1.4666..., 4/3 overall.
    let m = map(&[(0, 0), (240_000, 288_000), (480_000, 640_000)]).unwrap();
    assert!((m.segment_ratio(0) - 1.2).abs() < 1e-12);
    assert!((m.segment_ratio(1) - 352_000.0 / 240_000.0).abs() < 1e-12);
    let overall = m.output_frames().as_f64() / m.source_frames().as_f64();
    assert!((overall - 4.0 / 3.0).abs() < 1e-12);
}

#[test]
fn constant_map_rounds_the_endpoint_once() {
    let m = TimeMap::constant(48_000, 4.0 / 3.0).unwrap();
    assert_eq!(m.output_frames().get(), 64_000);
    assert!(m.check_ratio_range(0.5, 2.0).is_ok());
    assert!(m.check_ratio_range(0.5, 1.1).is_err());
}

#[test]
fn identity_is_recognised() {
    assert!(TimeMap::constant(1000, 1.0).unwrap().is_identity());
    assert!(!TimeMap::constant(1000, 1.5).unwrap().is_identity());
    assert!(TimeMap::empty().is_identity());
}

#[test]
fn editing_one_anchor_leaves_the_rest_alone() {
    let m = map(&[(0, 0), (1000, 1000), (2000, 2000)]).unwrap();
    let edited = m.with_anchor(WarpAnchor::user(1000, 1400)).unwrap();
    assert_eq!(edited.anchors().len(), 3);
    assert_eq!(edited.anchors()[1].output_frame, 1400);
    assert_eq!(edited.anchors()[2].output_frame, 2000);
    // A rejected edit leaves the original untouched.
    assert!(m.with_anchor(WarpAnchor::user(1000, 2500)).is_err());
    assert_eq!(m.anchors()[1].output_frame, 1000);
}

#[test]
fn beat_grid_maps_through_beats_not_one_bpm() {
    let rate = 48_000u32;
    let frames = rate as u64 * 4;
    let grid = BeatGrid::constant(120.0, rate, frames);
    let m = grid.map_to_tempo(90.0, rate, frames).unwrap();
    // 120 -> 90 BPM is alpha 4/3 on average.
    let overall = m.output_frames().as_f64() / m.source_frames().as_f64();
    assert!((overall - 4.0 / 3.0).abs() < 0.01, "overall ratio {overall}");
    // Every beat lands on the destination grid.
    let spb_out = 60.0 / 90.0 * rate as f64;
    for b in 1..8 {
        let src = b as f64 * 60.0 / 120.0 * rate as f64;
        let out = m.forward(src);
        assert!((out - b as f64 * spb_out).abs() < 2.0, "beat {b}: {out}");
    }
}

#[test]
fn document_rejects_a_map_that_does_not_cover_the_source() {
    let src = solfege::fixtures::sine(1000, 48_000, 440.0, 0.5, 1);
    let id = SourceIdentity::of(&src, 48_000);
    let mut doc = EditDocument::constant(id, 1.0, 0.0, EngineMode::Bypass);
    doc.anchors = vec![WarpAnchor::endpoint(0, 0), WarpAnchor::endpoint(500, 500)];
    assert!(doc.validate().is_err());
}

#[test]
fn document_round_trips_through_json() {
    let src = solfege::fixtures::sine(48_000, 48_000, 440.0, 0.5, 2);
    let id = SourceIdentity::of(&src, 48_000);
    let doc = EditDocument::constant(id, 1.5, -3.0, EngineMode::Polyphonic);
    let back = EditDocument::from_json(&doc.to_json()).unwrap();
    assert_eq!(doc, back);
    assert_eq!(doc.canonical_hash(), back.canonical_hash());
}

#[test]
fn canonical_hash_moves_when_an_edit_moves() {
    let src = solfege::fixtures::sine(48_000, 48_000, 440.0, 0.5, 1);
    let id = SourceIdentity::of(&src, 48_000);
    let a = EditDocument::constant(id.clone(), 1.5, 0.0, EngineMode::Polyphonic);
    let mut b = a.clone();
    b.pitch_semitones = 0.01;
    assert_ne!(a.canonical_hash(), b.canonical_hash());
    let c = EditDocument::constant(id, 1.5, 0.0, EngineMode::Polyphonic);
    assert_eq!(a.canonical_hash(), c.canonical_hash());
}
