//! Process contract, offline exactness and the quality gates the modes have
//! to hold (system-design.md sec.7 and sec.12, validation.md sec.4).

use solfege::analysis::{analyze, AnalysisSettings};
use solfege::audio::{AudioBuffer, AudioView, AudioViewMut};
use solfege::document::{EditDocument, EngineMode, FormantPolicy, QualityProfile};
use solfege::engines::{PreparedSeek, ProcessError, ProcessState};
use solfege::mapping::{TimeMap, WarpAnchor};
use solfege::plan::{build_engine, compile, CompileOptions};
use solfege::render::{render_offline, render_offline_random_blocks};
use solfege::{fixtures, metrics, PlanError, SourceIdentity};

const RATE: u32 = 48_000;

/// Every mode a user can pick that renders audio.
const MODES: [EngineMode; 6] = [
    EngineMode::ElasticPro,
    EngineMode::ElasticEfficient,
    EngineMode::Rhythmic,
    EngineMode::Soloist,
    EngineMode::Varispeed,
    EngineMode::Texture,
];

/// The modes that keep the sound natural and transpose independently.
const NATURAL: [EngineMode; 4] = [
    EngineMode::ElasticPro,
    EngineMode::ElasticEfficient,
    EngineMode::Rhythmic,
    EngineMode::Soloist,
];

fn doc_for(src: &AudioBuffer, alpha: f64, semis: f64, mode: EngineMode) -> EditDocument {
    let id = SourceIdentity::of(src, RATE);
    let mut d = EditDocument::constant(id, alpha, semis, mode);
    d.quality = QualityProfile::Offline;
    d
}

fn render_opts(
    src: &AudioBuffer,
    alpha: f64,
    semis: f64,
    mode: EngineMode,
    opts: &CompileOptions,
) -> (AudioBuffer, u64) {
    let doc = doc_for(src, alpha, semis, mode);
    let plan = compile(&doc, None, opts).expect("plan compiles");
    let (out, rep) = render_offline(&plan, src, 1024).expect("render succeeds");
    (out, rep.expected_frames)
}

fn render(src: &AudioBuffer, alpha: f64, semis: f64, mode: EngineMode) -> (AudioBuffer, u64) {
    render_opts(src, alpha, semis, mode, &CompileOptions::default())
}

/// Fundamental wander about its own mean, cents rms, over the steady middle.
fn pitch_wobble(out: &AudioBuffer, want: f64) -> f64 {
    let mut cents: Vec<f64> = Vec::new();
    let mut at = RATE as usize / 4;
    while at + 8192 + RATE as usize / 4 < out.frames() {
        if let Some(f) = metrics::dominant_hz(out, RATE, at, 8192) {
            cents.push(metrics::pitch_error_cents(f, want));
        }
        at += 4096;
    }
    assert!(cents.len() > 3, "not enough steady material");
    let m = cents.iter().sum::<f64>() / cents.len() as f64;
    (cents.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / cents.len() as f64).sqrt()
}

// ------------------------------------------------------------ the contract

#[test]
fn identity_bypass_is_sample_exact() {
    let src = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 2);
    let doc = EditDocument::identity(SourceIdentity::of(&src, RATE));
    assert!(doc.is_identity());
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    for block in [1usize, 17, 64, 127, 256, 1024] {
        let (out, _) = render_offline(&plan, &src, block).unwrap();
        assert!(metrics::is_bit_identical(&src, &out), "bypass differed at block {block}");
    }
}

#[test]
fn elastic_at_ratio_one_is_transparent() {
    // Not Bypass: the full engine, doing nothing. The phase-gradient kernel
    // keeps its rotation at zero when the hops agree, so what comes out is
    // the input to within float rounding.
    let src = fixtures::mixed(RATE as usize, RATE, 2);
    for mode in [EngineMode::ElasticPro, EngineMode::ElasticEfficient, EngineMode::Rhythmic] {
        let (out, _) = render(&src, 1.0, 0.0, mode);
        let d = metrics::rms_difference_db(&src, &out);
        assert!(d < -70.0, "{} at ratio 1 differs from the input by {d:.1} dBFS", mode.label());
    }
}

#[test]
fn every_mode_delivers_exactly_m_frames() {
    let tone = fixtures::harmonics(RATE as usize, RATE, 220.0, 6, 1);
    let drums = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
    for mode in MODES {
        let src = if mode == EngineMode::Rhythmic { &drums } else { &tone };
        for alpha in [0.5, 0.75, 0.9, 1.0, 1.1, 1.5, 2.0] {
            let (out, expected) = render(src, alpha, 0.0, mode);
            assert_eq!(out.frames() as u64, expected, "{} at alpha {alpha}", mode.label());
            assert!(
                !metrics::any_non_finite(&out),
                "{} produced non-finite output at alpha {alpha}",
                mode.label()
            );
        }
    }
}

#[test]
fn every_ratio_and_transposition_finishes() {
    // The combinations a live session wanders through. Any stall here is a
    // stream that goes silent mid-song.
    let src = fixtures::harmonics(RATE as usize * 2, RATE, 220.0, 8, 2);
    for mode in NATURAL {
        for alpha in [0.7, 1.0, 1.3, 1.6, 2.0] {
            for semis in [-12.0, -7.0, -3.0, 0.0, 2.0, 5.0, 12.0] {
                let doc = doc_for(&src, alpha, semis, mode);
                let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
                let (out, rep) = render_offline(&plan, &src, 512).unwrap_or_else(|e| {
                    panic!("{} at alpha {alpha}, {semis:+} st: {e}", mode.label())
                });
                assert_eq!(out.frames() as u64, rep.expected_frames);
            }
        }
    }
}

#[test]
fn block_size_does_not_change_the_result() {
    let src = fixtures::mixed(RATE as usize / 2, RATE, 2);
    for mode in [
        EngineMode::ElasticPro,
        EngineMode::Rhythmic,
        EngineMode::Soloist,
        EngineMode::Varispeed,
    ] {
        let semis = if mode == EngineMode::Varispeed { 0.0 } else { 3.0 };
        let doc = doc_for(&src, 1.5, semis, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let (reference, _) = render_offline(&plan, &src, 1024).unwrap();
        for block in [1usize, 17, 64, 127, 256, 4096] {
            let (other, _) = render_offline(&plan, &src, block).unwrap();
            let d = metrics::rms_difference_db(&reference, &other);
            assert!(d <= -100.0, "{} differed at block {block}: {d:.1} dBFS", mode.label());
        }
        let (rand, _) = render_offline_random_blocks(&plan, &src, 0xBADC0DE, 777).unwrap();
        let d = metrics::rms_difference_db(&reference, &rand);
        assert!(d <= -100.0, "{} differed on random blocks: {d:.1} dBFS", mode.label());
    }
}

#[test]
fn silence_in_silence_out() {
    let src = AudioBuffer::silence(2, RATE as usize / 2);
    for mode in MODES {
        let (out, _) = render(&src, 1.7, 0.0, mode);
        assert_eq!(out.peak(), 0.0, "{} invented energy in silence", mode.label());
    }
}

#[test]
fn empty_and_one_frame_sources_are_safe() {
    for frames in [0usize, 1, 7] {
        let src = AudioBuffer::silence(1, frames);
        let doc = EditDocument::identity(SourceIdentity::of(&src, RATE));
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let (out, rep) = render_offline(&plan, &src, 64).unwrap();
        assert_eq!(out.frames(), frames);
        assert_eq!(rep.produced_frames, frames as u64);
    }
    // And through a real engine, not just Bypass.
    for frames in [1usize, 7, 300] {
        let src = fixtures::sine(frames, RATE, 440.0, 0.5, 1);
        for mode in NATURAL {
            let (out, expected) = render(&src, 1.5, 2.0, mode);
            assert_eq!(out.frames() as u64, expected, "{} on {frames} frames", mode.label());
        }
    }
}

#[test]
fn process_reports_typed_errors_not_panics() {
    let src = fixtures::sine(RATE as usize / 4, RATE, 440.0, 0.5, 1);
    let doc = doc_for(&src, 1.5, 0.0, EngineMode::ElasticPro);
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    let mut engine = build_engine(&plan).unwrap();

    let big = vec![vec![0.0f32; plan.cfg.max_block + 1]];
    let mut out = vec![vec![0.0f32; 64]];
    let err = engine
        .process(
            AudioView::from_planar(&big, 0, plan.cfg.max_block + 1).unwrap(),
            AudioViewMut::from_planar(&mut out, 0, 64).unwrap(),
            false,
        )
        .unwrap_err();
    assert!(matches!(err, ProcessError::BlockTooLarge { .. }));

    let stereo = vec![vec![0.0f32; 64], vec![0.0f32; 64]];
    let err = engine
        .process(
            AudioView::from_planar(&stereo, 0, 64).unwrap(),
            AudioViewMut::from_planar(&mut out, 0, 64).unwrap(),
            false,
        )
        .unwrap_err();
    assert!(matches!(err, ProcessError::ChannelMismatch { .. }));

    let mono = vec![vec![0.0f32; 64]];
    engine
        .process(
            AudioView::from_planar(&mono, 0, 64).unwrap(),
            AudioViewMut::from_planar(&mut out, 0, 64).unwrap(),
            true,
        )
        .unwrap();
    let err = engine
        .process(
            AudioView::from_planar(&mono, 0, 64).unwrap(),
            AudioViewMut::from_planar(&mut out, 0, 64).unwrap(),
            false,
        )
        .unwrap_err();
    assert_eq!(err, ProcessError::InputAfterEndOfInput);
}

#[test]
fn a_no_progress_call_says_what_it_waits_for() {
    let src = fixtures::sine(RATE as usize / 4, RATE, 440.0, 0.5, 1);
    for mode in NATURAL {
        let doc = doc_for(&src, 1.5, 0.0, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let mut engine = build_engine(&plan).unwrap();
        let empty: Vec<Vec<f32>> = vec![Vec::new()];
        let mut out = vec![vec![0.0f32; 256]];
        let report = engine
            .process(
                AudioView::from_planar(&empty, 0, 0).unwrap(),
                AudioViewMut::from_planar(&mut out, 0, 256).unwrap(),
                false,
            )
            .unwrap();
        assert_eq!(report.consumed_frames, 0);
        assert_eq!(report.produced_frames, 0);
        assert_eq!(report.state, ProcessState::NeedInput, "{}", mode.label());
    }
}

#[test]
fn draining_finishes_and_consumes_everything_once() {
    let src = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    for mode in NATURAL {
        let doc = doc_for(&src, 1.75, 2.0, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let mut engine = build_engine(&plan).unwrap();
        let expected = plan.output_frames();

        let block = 333usize;
        let mut in_pos = 0usize;
        let mut out_pos = 0u64;
        let mut scratch = vec![vec![0.0f32; block]];
        let mut calls = 0;
        while out_pos < expected {
            let want = (src.frames() - in_pos).min(block);
            let eoi = in_pos + want >= src.frames();
            let report = engine
                .process(
                    AudioView::from_planar(src.planes(), in_pos, want).unwrap(),
                    AudioViewMut::from_planar(&mut scratch, 0, block).unwrap(),
                    eoi,
                )
                .unwrap();
            assert!(report.consumed_frames <= want);
            assert!(report.produced_frames <= block);
            assert_eq!(report.output_start_frame, out_pos);
            in_pos += report.consumed_frames;
            out_pos += report.produced_frames as u64;
            if report.state == ProcessState::Finished {
                break;
            }
            calls += 1;
            assert!(calls < 100_000, "{} did not terminate", mode.label());
        }
        assert_eq!(out_pos, expected, "{}", mode.label());
        assert_eq!(in_pos, src.frames(), "{}: input not consumed exactly once", mode.label());
    }
}

#[test]
fn seek_lands_on_the_right_logical_frame() {
    let src = fixtures::harmonics(RATE as usize, RATE, 220.0, 6, 1);
    for mode in NATURAL {
        let doc = doc_for(&src, 1.5, 0.0, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let mut engine = build_engine(&plan).unwrap();
        let target = plan.output_frames() / 3;
        engine.reset(&PreparedSeek { output_frame: target, preroll: 0 });
        let from = engine.input_position() as usize;
        let mut scratch = vec![vec![0.0f32; 512]];
        let report = engine
            .process(
                AudioView::from_planar(src.planes(), from, 4096).unwrap(),
                AudioViewMut::from_planar(&mut scratch, 0, 512).unwrap(),
                false,
            )
            .unwrap();
        assert_eq!(report.output_start_frame, target, "{}", mode.label());
    }
}

/// A `reset` to the start must leave the engine exactly as `prepare` did: the
/// real-time path always seeks before it plays.
#[test]
fn reset_to_zero_matches_a_fresh_prepare() {
    let src = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    for mode in MODES {
        let semis = if mode == EngineMode::Varispeed { 0.0 } else { 2.0 };
        let doc = doc_for(&src, 1.5, semis, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let (reference, _) = render_offline(&plan, &src, 1024).unwrap();

        let mut engine = build_engine(&plan).unwrap();
        engine.reset(&PreparedSeek { output_frame: 0, preroll: 0 });
        assert_eq!(engine.input_position(), 0, "{} after a reset to zero", mode.label());
        let (after, rep) = solfege::render::render_with(&mut *engine, &plan, &src, 1024).unwrap();
        assert_eq!(rep.produced_frames, rep.expected_frames, "{}", mode.label());
        let d = metrics::rms_difference_db(&reference, &after);
        assert!(d <= -100.0, "{} differed after a reset to zero: {d:.1} dBFS", mode.label());
    }
}

/// Every seek position, every transposition: the engine must start
/// producing again. Live playback seeks on every play and every mode change.
#[test]
fn seeking_with_a_transpose_keeps_producing() {
    let src = fixtures::mixed(RATE as usize * 2, RATE, 2);
    for mode in NATURAL {
        for semis in [2.0f64, 3.0, -5.0] {
            let doc = doc_for(&src, 1.3, semis, mode);
            let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
            let total = plan.output_frames();
            for num in [1u64, 3, 7, 11, 17, 29] {
                let target = total * num / 32;
                let mut engine = build_engine(&plan).unwrap();
                engine.reset(&PreparedSeek { output_frame: target, preroll: 0 });
                let mut in_pos = engine.input_position() as usize;
                let block = 1024usize;
                let mut scratch = vec![vec![0.0f32; block]; plan.cfg.channels];
                let mut produced = 0usize;
                let mut calls = 0;
                // Enough input for the engine's look-ahead, never the whole
                // file: an engine that only produces at end-of-input fails.
                while produced == 0 && calls < 60 {
                    let want = (src.frames().saturating_sub(in_pos)).min(block);
                    let eoi = in_pos + want >= src.frames();
                    let report = engine
                        .process(
                            AudioView::from_planar(src.planes(), in_pos, want).unwrap(),
                            AudioViewMut::from_planar(&mut scratch, 0, block).unwrap(),
                            eoi,
                        )
                        .unwrap();
                    in_pos += report.consumed_frames;
                    produced += report.produced_frames;
                    calls += 1;
                }
                assert!(
                    produced > 0,
                    "{} at {semis:+} semitones produced nothing after a seek to {target} \
                     ({calls} calls)",
                    mode.label()
                );
            }
        }
    }
}

// ----------------------------------------------------------------- quality

/// A steady tone must come out steady: no breathing in level.
#[test]
fn a_steady_tone_does_not_breathe() {
    let src = fixtures::harmonics(RATE as usize * 2, RATE, 220.0, 8, 2);
    for mode in NATURAL {
        for semis in [0.0, 4.0] {
            let (out, _) = render(&src, 1.3, semis, mode);
            let env = metrics::envelope_db(&out, 2048, 512);
            let a = env.len() / 6;
            let b = env.len() - env.len() / 6;
            let m = env[a..b].iter().sum::<f64>() / (b - a) as f64;
            let rms =
                (env[a..b].iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (b - a) as f64).sqrt();
            assert!(rms < 0.4, "{} at {semis:+} st breathes {rms:.3} dB rms", mode.label());
        }
    }
}

/// Bass holds its pitch: the failure the old spectral engine was patched
/// for three times.
#[test]
fn bass_notes_hold_still() {
    for hz in [55.0f64, 82.5] {
        let src = fixtures::harmonics(RATE as usize * 3, RATE, hz, 8, 2);
        for mode in [EngineMode::ElasticPro, EngineMode::ElasticEfficient, EngineMode::Soloist] {
            for (alpha, semis) in [(1.0, 3.0), (1.5, 0.0), (1.3, -5.0)] {
                let (out, _) = render(&src, alpha, semis, mode);
                let want = hz * 2f64.powf(semis / 12.0);
                let w = pitch_wobble(&out, want);
                assert!(
                    w < 1.5,
                    "{} wobbles {w:.2} cents on {hz} Hz at alpha {alpha}, {semis:+} st",
                    mode.label()
                );
            }
        }
    }
}

/// The attack of a drum hit survives a stretch and a transposition: same
/// rise time, nothing smeared in front of it, and it lands where the map
/// says.
#[test]
fn attacks_stay_sharp_and_on_time() {
    let period = RATE as usize / 4;
    let src = fixtures::drum_hits(RATE as usize * 3, RATE, period, 2);
    let src_peaks = metrics::peak_positions(&src, 0.5, period / 2);
    // Each hit is a different noise burst, so rise times differ hit by hit
    // (0.8..2 ms here): compare every output hit with its own source hit.
    let src_rise: Vec<f64> =
        src_peaks.iter().map(|q| metrics::attack_rise_ms(&src, *q, RATE).unwrap()).collect();

    for mode in [EngineMode::ElasticPro, EngineMode::ElasticEfficient, EngineMode::Rhythmic] {
        for (alpha, semis) in [(1.5, 0.0), (2.0, 0.0), (0.75, 0.0), (1.0, 3.0), (1.5, -5.0)] {
            let doc = doc_for(&src, alpha, semis, mode);
            let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
            let (out, _) = render_offline(&plan, &src, 1024).unwrap();
            let out_peaks = metrics::peak_positions(&out, 0.5, period / 2);
            assert_eq!(out_peaks.len(), src_peaks.len(), "{} lost or doubled a hit", mode.label());
            for (i, q) in out_peaks.iter().enumerate().skip(1) {
                let rise = metrics::attack_rise_ms(&out, *q, RATE).unwrap();
                // Transposing down by resampling slows the attack by 1/p;
                // that is the pitch change, not smear.
                let allowed = src_rise[i] * 2f64.powf(-semis / 12.0).max(1.0) * 1.3 + 0.5;
                let pre = metrics::pre_attack_db(&out, *q, RATE).unwrap_or(-120.0);
                let want = plan.cfg.map.forward(src_peaks[i] as f64);
                let err_ms = (*q as f64 - want).abs() * 1000.0 / RATE as f64;
                let tag = format!("{} at alpha {alpha}, {semis:+} st, hit {i}", mode.label());
                assert!(rise < allowed, "{tag}: rise {rise:.2} ms, source {:.2}", src_rise[i]);
                assert!(pre < -60.0, "{tag}: pre-echo {pre:.1} dB");
                assert!(err_ms < 6.0, "{tag}: landed {err_ms:.2} ms off the map");
            }
        }
    }
}

/// Transient handling moves where frames are read; it never changes a gain.
#[test]
fn transient_handling_does_not_duck_the_level() {
    let src = fixtures::mixed(RATE as usize * 3, RATE, 2);
    for mode in [EngineMode::ElasticPro, EngineMode::Rhythmic] {
        let on = CompileOptions::default();
        let off = CompileOptions { transient_protect: false, ..CompileOptions::default() };
        let (a, _) = render_opts(&src, 1.5, 2.0, mode, &on);
        let (b, _) = render_opts(&src, 1.5, 2.0, mode, &off);
        assert!(!metrics::is_bit_identical(&a, &b), "{}: the switch does nothing", mode.label());
        let la = metrics::level(&a);
        let lb = metrics::level(&b);
        assert!(
            (la.rms_dbfs - lb.rms_dbfs).abs() < 0.6,
            "{}: transient handling moved the level {:.2} -> {:.2} dBFS",
            mode.label(),
            lb.rms_dbfs,
            la.rms_dbfs
        );
    }
}

#[test]
fn stretching_alone_does_not_move_the_pitch() {
    let src = fixtures::sine(RATE as usize, RATE, 440.0, 0.5, 1);
    for mode in NATURAL {
        for alpha in [0.6, 1.4, 2.0] {
            let (out, _) = render(&src, alpha, 0.0, mode);
            let hz = metrics::dominant_hz(&out, RATE, out.frames() / 3, 8192).unwrap();
            let cents = metrics::pitch_error_cents(hz, 440.0);
            assert!(cents.abs() < 5.0, "{} at alpha {alpha}: {cents:+.2} cents", mode.label());
        }
    }
}

#[test]
fn transposition_hits_the_requested_interval() {
    let tone = fixtures::harmonics(RATE as usize, RATE, 220.0, 6, 1);
    for mode in NATURAL {
        for semis in [-12.0, -7.0, -1.0, 1.0, 7.0, 12.0] {
            let (out, _) = render(&tone, 1.0, semis, mode);
            let expected = 220.0 * 2f64.powf(semis / 12.0);
            let hz = metrics::dominant_hz(&out, RATE, out.frames() / 3, 8192).unwrap();
            let cents = metrics::pitch_error_cents(hz, expected);
            assert!(cents.abs() < 5.0, "{} {semis:+} st: {cents:+.2} cents", mode.label());
        }
    }
}

#[test]
fn varispeed_pitch_follows_the_ratio() {
    let src = fixtures::sine(RATE as usize, RATE, 440.0, 0.5, 1);
    let (out, _) = render(&src, 2.0, 0.0, EngineMode::Varispeed);
    let hz = metrics::dominant_hz(&out, RATE, RATE as usize / 2, 8192).unwrap();
    let cents = metrics::pitch_error_cents(hz, 220.0);
    assert!(cents.abs() < 5.0, "varispeed produced {hz:.2} Hz ({cents:+.2} cents from 220)");
}

/// Formants: with Preserve the vowel's resonances stay where they were while
/// the pitch moves; with FollowPitch they move with it.
#[test]
fn formants_follow_the_policy() {
    // Envelope estimate: energy centroid of the band around the first
    // formant. Coarse, but it moves by a factor of `p` when the envelope
    // follows and stays put when it does not.
    fn centroid(buf: &AudioBuffer, lo: f64, hi: f64) -> f64 {
        let n = 16384;
        let mut stft = solfege::dsp::stft::RealStft::new(n);
        let w = solfege::dsp::window::hann_periodic(n);
        let mono = buf.mono_sum();
        let s = mono.len() / 2 - n / 2;
        let time: Vec<f32> = (0..n).map(|i| mono[s + i] * w[i]).collect();
        let mut spec = vec![realfft::num_complex::Complex32::new(0.0, 0.0); n / 2 + 1];
        stft.forward(&time, &mut spec);
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for (k, c) in spec.iter().enumerate() {
            let hz = k as f64 * RATE as f64 / n as f64;
            if hz >= lo && hz <= hi {
                num += hz * c.norm_sqr() as f64;
                den += c.norm_sqr() as f64;
            }
        }
        num / den
    }
    let src = fixtures::vowel(RATE as usize * 2, RATE, 130.0, [730.0, 1090.0, 2440.0], 1);
    let base = centroid(&src, 300.0, 1400.0);
    for mode in [EngineMode::ElasticPro, EngineMode::Soloist] {
        for semis in [-5.0f64, 5.0] {
            let p = 2f64.powf(semis / 12.0);
            let mut doc = doc_for(&src, 1.0, semis, mode);
            doc.formant = FormantPolicy::Preserve;
            let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
            let (kept, _) = render_offline(&plan, &src, 1024).unwrap();
            doc.formant = FormantPolicy::FollowPitch;
            let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
            let (moved, _) = render_offline(&plan, &src, 1024).unwrap();
            let ck = centroid(&kept, 300.0, 1400.0);
            let cm = centroid(&moved, 300.0 * p, 1400.0 * p);
            assert!(
                (ck / base - 1.0).abs() < 0.12,
                "{} {semis:+} st Preserve moved the formant region {base:.0} -> {ck:.0} Hz",
                mode.label()
            );
            assert!(
                (cm / (base * p) - 1.0).abs() < 0.12,
                "{} {semis:+} st FollowPitch put it at {cm:.0} Hz, want {:.0}",
                mode.label(),
                base * p
            );
            // Moving the envelope must not change loudness much.
            let lk = metrics::level(&kept).rms_dbfs;
            let ls = metrics::level(&src).rms_dbfs;
            assert!((lk - ls).abs() < 3.0, "{} Preserve changed level by {:.1} dB", mode.label(), lk - ls);
        }
    }
}

/// One rotation per bin, one set of grain positions: whatever relation the
/// channels came in with, they leave with.
#[test]
fn stereo_relationships_survive_the_engine() {
    let mono = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 1);
    for mode in NATURAL {
        let identical = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 2);
        let (out, _) = render(&identical, 1.4, 3.0, mode);
        assert_eq!(out.channel(0), out.channel(1), "{} split an identical pair", mode.label());

        let inverted = fixtures::inverted_stereo(&mono);
        let (out, _) = render(&inverted, 1.4, 3.0, mode);
        let corr = metrics::channel_correlation(&out).unwrap();
        assert!(corr < -0.95, "{} lost the inversion: {corr:+.4}", mode.label());

        // A fixed microphone delay is the stereo image; stretching must not
        // move it. (Transposing scales it with the resampling, which is a
        // property of the method, not a per-channel decision.)
        let hits = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
        let delayed = fixtures::delayed_stereo(&hits, 24);
        let (out, _) = render(&delayed, 1.4, 0.0, mode);
        let lag = metrics::interchannel_lag(&out, RATE as usize / 2, 16384, 200).unwrap();
        assert!((lag - 24).abs() <= 2, "{} moved a 24-frame delay to {lag}", mode.label());
    }
}

#[test]
fn a_hard_anchor_lands_where_it_was_put() {
    let n = RATE as usize;
    let src = fixtures::impulse_train(n, RATE as usize / 4, 1, 0);
    for mode in [EngineMode::ElasticPro, EngineMode::Rhythmic] {
        let mut doc = doc_for(&src, 1.0, 0.0, mode);
        let mid_src = RATE as u64 / 2;
        let mid_out = mid_src + RATE as u64 / 20;
        doc.anchors = vec![
            WarpAnchor::endpoint(0, 0),
            WarpAnchor::user(mid_src, mid_out),
            WarpAnchor::endpoint(n as u64, n as u64),
        ];
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let (out, _) = render_offline(&plan, &src, 1024).unwrap();
        let peaks = metrics::peak_positions(&out, 0.4, RATE as usize / 16);
        let nearest = peaks
            .iter()
            .map(|p| (*p as i64 - mid_out as i64).abs())
            .min()
            .expect("some impulses survived");
        let ms = nearest as f64 * 1000.0 / RATE as f64;
        assert!(ms <= 1.0, "{}: nearest impulse {ms:.3} ms from the anchor", mode.label());
    }
}

// ----------------------------------------------------------- live changes

/// Retargeting a running engine keeps going from where it was, at the new
/// ratio and pitch, without a gap.
#[test]
fn retarget_continues_seamlessly() {
    let src = fixtures::harmonics(RATE as usize * 3, RATE, 220.0, 8, 1);
    for mode in NATURAL {
        let doc = doc_for(&src, 1.0, 0.0, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let mut engine = build_engine(&plan).unwrap();
        let block = 512usize;
        let mut scratch = vec![vec![0.0f32; block]];
        let mut in_pos = 0usize;
        let mut out: Vec<f32> = Vec::new();
        let mut switched = false;
        for _ in 0..400 {
            if !switched && out.len() >= RATE as usize {
                let mut map = TimeMap::constant(src.frames() as u64, 1.5).unwrap();
                let pos = engine
                    .retarget(&mut map, 2f64.powf(4.0 / 12.0), FormantPolicy::FollowPitch)
                    .expect("natural modes retarget");
                // The old map comes back for disposal off the audio thread.
                assert_eq!(map.output_frames().get(), src.frames() as u64);
                assert!(pos > 0);
                switched = true;
            }
            let want = (src.frames() - in_pos).min(block);
            let eoi = in_pos + want >= src.frames();
            let r = engine
                .process(
                    AudioView::from_planar(src.planes(), in_pos, want).unwrap(),
                    AudioViewMut::from_planar(&mut scratch, 0, block).unwrap(),
                    eoi,
                )
                .unwrap();
            in_pos += r.consumed_frames;
            out.extend_from_slice(&scratch[0][..r.produced_frames]);
            if r.state == ProcessState::Finished {
                break;
            }
        }
        assert!(switched);
        let buf = AudioBuffer::from_planar(vec![out]).unwrap();
        // No block of the steady tone drops out across the change.
        let env = metrics::envelope_db(&buf, 1024, 256);
        let body = &env[8..env.len() - 8];
        let m = body.iter().sum::<f64>() / body.len() as f64;
        let worst = body.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(worst > m - 3.0, "{}: level fell to {worst:.1} dB (mean {m:.1})", mode.label());
        // And the new pitch is what comes out afterwards.
        let hz = metrics::dominant_hz(&buf, RATE, RATE as usize + RATE as usize / 2, 8192).unwrap();
        let want = 220.0 * 2f64.powf(4.0 / 12.0);
        let cents = metrics::pitch_error_cents(hz, want);
        assert!(cents.abs() < 5.0, "{}: {hz:.1} Hz after retarget, want {want:.1}", mode.label());
    }
}

// ---------------------------------------------------------------- compiler

#[test]
fn compiler_rejects_before_it_renders() {
    let src = fixtures::harmonics(RATE as usize / 4, RATE, 220.0, 6, 1);

    let doc = doc_for(&src, 1.5, 3.0, EngineMode::Varispeed);
    assert!(matches!(
        compile(&doc, None, &CompileOptions::default()),
        Err(PlanError::TapePitchConflict { .. })
    ));

    let doc = doc_for(&src, 6.0, 0.0, EngineMode::Soloist);
    assert!(matches!(
        compile(&doc, None, &CompileOptions::default()),
        Err(PlanError::InternalRatio { .. })
    ));

    let doc = doc_for(&src, 1.0, 30.0, EngineMode::ElasticPro);
    assert!(compile(&doc, None, &CompileOptions::default()).is_err());

    let mut doc = doc_for(&src, 1.2, 2.0, EngineMode::Texture);
    doc.formant = FormantPolicy::Preserve;
    assert!(matches!(
        compile(&doc, None, &CompileOptions::default()),
        Err(PlanError::FormantUnsupported { .. })
    ));
    doc.mode = EngineMode::ElasticPro;
    assert!(compile(&doc, None, &CompileOptions::default()).is_ok());
    doc.mode = EngineMode::Soloist;
    assert!(compile(&doc, None, &CompileOptions::default()).is_ok());
}

#[test]
fn documents_from_the_old_mode_names_still_load() {
    let src = fixtures::sine(1000, RATE, 440.0, 0.5, 1);
    let doc = doc_for(&src, 1.2, 0.0, EngineMode::ElasticPro);
    let json = doc.to_json();
    for (old, new) in [
        ("polyphonic", EngineMode::ElasticPro),
        ("hybrid", EngineMode::ElasticPro),
        ("percussive", EngineMode::Rhythmic),
        ("monophonic", EngineMode::Soloist),
        ("tape", EngineMode::Varispeed),
    ] {
        let legacy = json.replace("\"elastic_pro\"", &format!("\"{old}\""));
        let back = EditDocument::from_json(&legacy).expect("legacy document loads");
        assert_eq!(back.mode, new, "{old}");
        assert_eq!(EngineMode::parse(old), Some(new));
    }
}

#[test]
fn a_group_with_mismatched_members_is_rejected() {
    use solfege::plan::check_group;
    let a_src = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
    let b_src = fixtures::drum_hits(RATE as usize / 2, RATE, RATE as usize / 4, 1);
    let a = doc_for(&a_src, 1.5, 0.0, EngineMode::Rhythmic);
    let b = doc_for(&b_src, 1.5, 0.0, EngineMode::Rhythmic);
    assert!(check_group(&[&a, &a]).is_ok());
    assert!(check_group(&[&a, &b]).is_err());
}

#[test]
fn a_seeded_fx_render_repeats_exactly() {
    let src = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    let mut doc = doc_for(&src, 2.0, 0.0, EngineMode::Texture);
    doc.deterministic_seed = 42;
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    let (a, _) = render_offline(&plan, &src, 1024).unwrap();
    let (b, _) = render_offline(&plan, &src, 1024).unwrap();
    assert!(metrics::is_bit_identical(&a, &b));

    doc.deterministic_seed = 43;
    let plan2 = compile(&doc, None, &CompileOptions::default()).unwrap();
    assert_ne!(plan.render_key, plan2.render_key);
    let (c, _) = render_offline(&plan2, &src, 1024).unwrap();
    assert!(!metrics::is_bit_identical(&a, &c));
}

#[test]
fn auto_routes_material_to_the_mode_built_for_it() {
    let drums = fixtures::drum_hits(RATE as usize * 2, RATE, RATE as usize / 4, 1);
    let doc = doc_for(&drums, 1.5, 0.0, EngineMode::Auto);
    let a = analyze(&drums, RATE, "test", &AnalysisSettings::default());
    let plan = compile(&doc, Some(&a), &CompileOptions::default()).unwrap();
    assert_ne!(plan.mode, EngineMode::Auto);
    assert!(plan.auto_reason.is_some(), "auto must say why it chose a mode");

    // Without analysis Auto still picks something that handles anything.
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    assert_eq!(plan.mode, EngineMode::ElasticPro);
}

#[test]
fn the_render_is_not_normalised_behind_the_users_back() {
    let quiet = fixtures::sine(RATE as usize / 2, RATE, 440.0, 0.05, 1);
    for mode in NATURAL {
        let (out, _) = render(&quiet, 1.5, 0.0, mode);
        assert!(out.peak() < 0.2, "{} normalised the output: peak {}", mode.label(), out.peak());
    }
}
