//! Process contract and offline exactness (system-design.md sec.7 and sec.12,
//! validation.md sec.4).

use solfege::analysis::{analyze, AnalysisSettings};
use solfege::audio::{AudioBuffer, AudioView, AudioViewMut};
use solfege::document::{EditDocument, EngineMode, FormantPolicy, QualityProfile};
use solfege::engines::{PreparedSeek, ProcessError, ProcessState};
use solfege::mapping::WarpAnchor;
use solfege::plan::{build_engine, compile, merge_protections, sustain_ratio, CompileOptions};
use solfege::render::{render_offline, render_offline_random_blocks};
use solfege::{fixtures, metrics, PlanError, SourceIdentity};

const RATE: u32 = 48_000;

fn doc_for(src: &AudioBuffer, alpha: f64, semis: f64, mode: EngineMode) -> EditDocument {
    let id = SourceIdentity::of(src, RATE);
    let mut d = EditDocument::constant(id, alpha, semis, mode);
    d.quality = QualityProfile::Offline;
    d
}

fn render(
    src: &AudioBuffer,
    alpha: f64,
    semis: f64,
    mode: EngineMode,
    block: usize,
) -> (AudioBuffer, u64) {
    let doc = doc_for(src, alpha, semis, mode);
    let a = analyze(src, RATE, "test", &AnalysisSettings::default());
    let plan = compile(&doc, Some(&a), &CompileOptions::default()).expect("plan compiles");
    let (out, rep) = render_offline(&plan, src, block).expect("render succeeds");
    (out, rep.expected_frames)
}

#[test]
fn identity_bypass_is_sample_exact() {
    let src = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 2);
    let doc = EditDocument::identity(SourceIdentity::of(&src, RATE));
    assert!(doc.is_identity());
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    for block in [1usize, 17, 64, 127, 256, 1024] {
        let (out, _) = render_offline(&plan, &src, block).unwrap();
        assert!(
            metrics::is_bit_identical(&src, &out),
            "bypass differed at block {block}"
        );
    }
}

#[test]
fn every_mode_delivers_exactly_m_frames() {
    let tone = fixtures::harmonics(RATE as usize, RATE, 220.0, 6, 1);
    let drums = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
    for mode in [
        EngineMode::Tape,
        EngineMode::Monophonic,
        EngineMode::Polyphonic,
        EngineMode::Hybrid,
        EngineMode::Percussive,
        EngineMode::Texture,
    ] {
        let src = if mode == EngineMode::Percussive { &drums } else { &tone };
        for alpha in [0.5, 0.75, 0.9, 1.0, 1.1, 1.5, 2.0] {
            let (out, expected) = render(src, alpha, 0.0, mode, 1024);
            assert_eq!(
                out.frames() as u64,
                expected,
                "{} at alpha {alpha}",
                mode.label()
            );
            assert!(
                !metrics::any_non_finite(&out),
                "{} produced non-finite output at alpha {alpha}",
                mode.label()
            );
        }
    }
}

#[test]
fn block_size_does_not_change_the_result() {
    let src = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    for mode in [EngineMode::Monophonic, EngineMode::Polyphonic, EngineMode::Tape] {
        let doc = doc_for(&src, 1.5, 0.0, mode);
        let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
        let (reference, _) = render_offline(&plan, &src, 1024).unwrap();
        for block in [1usize, 17, 64, 127, 256, 4096] {
            let (other, _) = render_offline(&plan, &src, block).unwrap();
            let d = metrics::rms_difference_db(&reference, &other);
            assert!(
                d <= -100.0,
                "{} differed at block {block}: {d:.1} dBFS",
                mode.label()
            );
        }
        let (rand, _) = render_offline_random_blocks(&plan, &src, 0xBADC0DE, 777).unwrap();
        let d = metrics::rms_difference_db(&reference, &rand);
        assert!(d <= -100.0, "{} differed on random blocks: {d:.1} dBFS", mode.label());
    }
}

#[test]
fn silence_in_silence_out() {
    let src = AudioBuffer::silence(2, RATE as usize / 2);
    for mode in [EngineMode::Monophonic, EngineMode::Polyphonic, EngineMode::Tape] {
        let (out, _) = render(&src, 1.7, 0.0, mode, 512);
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
}

#[test]
fn process_reports_typed_errors_not_panics() {
    let src = fixtures::sine(RATE as usize / 4, RATE, 440.0, 0.5, 1);
    let doc = doc_for(&src, 1.5, 0.0, EngineMode::Monophonic);
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    let mut engine = build_engine(&plan).unwrap();

    // Block larger than the prepared maximum.
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

    // Wrong channel count.
    let stereo = vec![vec![0.0f32; 64], vec![0.0f32; 64]];
    let err = engine
        .process(
            AudioView::from_planar(&stereo, 0, 64).unwrap(),
            AudioViewMut::from_planar(&mut out, 0, 64).unwrap(),
            false,
        )
        .unwrap_err();
    assert!(matches!(err, ProcessError::ChannelMismatch { .. }));

    // Input after end-of-input.
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
    let doc = doc_for(&src, 1.5, 0.0, EngineMode::Polyphonic);
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
    assert_eq!(report.state, ProcessState::NeedInput);
}

#[test]
fn draining_finishes_and_consumes_everything_once() {
    let src = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    let doc = doc_for(&src, 1.75, 0.0, EngineMode::Polyphonic);
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    let mut engine = build_engine(&plan).unwrap();
    let expected = plan.output_frames();

    let block = 333usize;
    let mut in_pos = 0usize;
    let mut out_pos = 0u64;
    let mut scratch = vec![vec![0.0f32; block]];
    let mut calls = 0;
    let mut saw_finished = false;
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
            saw_finished = true;
            break;
        }
        calls += 1;
        assert!(calls < 100_000, "engine did not terminate");
    }
    assert!(saw_finished || out_pos == expected);
    assert_eq!(out_pos, expected);
    assert_eq!(in_pos, src.frames(), "input was not fully consumed exactly once");
}

#[test]
fn seek_lands_on_the_right_logical_frame() {
    let src = fixtures::harmonics(RATE as usize, RATE, 220.0, 6, 1);
    let doc = doc_for(&src, 1.5, 0.0, EngineMode::Monophonic);
    let plan = compile(&doc, None, &CompileOptions::default()).unwrap();
    let mut engine = build_engine(&plan).unwrap();
    let target = plan.output_frames() / 3;
    engine.reset(&PreparedSeek { output_frame: target, preroll: 0 });

    // Feed from the source position the map says the target reads from.
    let s = plan.cfg.map.inverse(target as f64).floor() as usize;
    let mut scratch = vec![vec![0.0f32; 512]];
    let report = engine
        .process(
            AudioView::from_planar(src.planes(), s.saturating_sub(4096), 4096).unwrap(),
            AudioViewMut::from_planar(&mut scratch, 0, 512).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(report.output_start_frame, target);
}

/// A `reset` to the start must leave the engine exactly as `prepare` did.
///
/// The real-time path always seeks before it plays, so a reset that is even
/// slightly off from a fresh prepare shows up as a stream that never reports
/// Finished and underruns forever. Offline never seeks, so nothing else here
/// would catch it.
#[test]
fn reset_to_zero_matches_a_fresh_prepare() {
    let src = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    for mode in [
        EngineMode::Tape,
        EngineMode::Monophonic,
        EngineMode::Polyphonic,
        EngineMode::Hybrid,
        EngineMode::Percussive,
        EngineMode::Texture,
    ] {
        let doc = doc_for(&src, 1.5, 0.0, mode);
        let a = analyze(&src, RATE, "test", &AnalysisSettings::default());
        let plan = compile(&doc, Some(&a), &CompileOptions::default()).unwrap();
        let (reference, _) = render_offline(&plan, &src, 1024).unwrap();

        let mut engine = build_engine(&plan).unwrap();
        engine.reset(&PreparedSeek { output_frame: 0, preroll: 0 });
        assert_eq!(
            engine.input_position(),
            0,
            "{} wants input from the wrong place after a reset to zero",
            mode.label()
        );
        let (after_reset, rep) =
            solfege::render::render_with(&mut *engine, &plan, &src, 1024).unwrap();
        assert_eq!(
            rep.produced_frames, rep.expected_frames,
            "{} stopped short after a reset",
            mode.label()
        );
        let d = metrics::rms_difference_db(&reference, &after_reset);
        assert!(
            d <= -100.0,
            "{} differed after a reset to zero: {d:.1} dBFS",
            mode.label()
        );
    }
}

/// The overlap-add has to sum flat, or every steady sound breathes.
///
/// sqrt-Hann on both analysis and synthesis at a quarter-window hop gives a
/// Hann product whose overlapped sum is exactly 2, and the engine divides by
/// the sum it actually accumulated rather than by that constant. This checks
/// both halves: the window really is COLA at the hop used, and the running
/// divisor really does flatten it.
#[test]
fn overlap_add_sums_flat() {
    use solfege::dsp::window::sqrt_hann_periodic;
    for n in [1024usize, 2048, 4096] {
        let hop = n / 4;
        let w = sqrt_hann_periodic(n);
        let span = n * 4;
        let mut acc = vec![0.0f64; span];
        let mut start = 0usize;
        while start + n <= span {
            for i in 0..n {
                acc[start + i] += (w[i] * w[i]) as f64;
            }
            start += hop;
        }
        // Only the fully overlapped interior is expected to be flat.
        let lo = n;
        let hi = span - n;
        // Tolerance is set by the `f32` window table, not by the arithmetic:
        // the ripple below is about -145 dB, which is flat by any measure that
        // matters and is as flat as a single-precision window can be.
        let first = acc[lo];
        for (i, v) in acc[lo..hi].iter().enumerate() {
            assert!(
                (v - first).abs() < 1e-6,
                "window sum is not constant at n={n}, offset {i}: {v} vs {first}"
            );
        }
        assert!(
            (first - 2.0).abs() < 1e-6,
            "sqrt-Hann squared at hop n/4 should sum to 2, got {first}"
        );
    }
}

/// A steady tone must come out steady. This is the end-to-end version of the
/// COLA check: any normalisation error shows up as the envelope breathing.
#[test]
fn a_steady_tone_does_not_breathe() {
    let src = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 2);
    for mode in [EngineMode::Polyphonic, EngineMode::Hybrid, EngineMode::Monophonic] {
        let (out, _) = render(&src, 1.3, 0.0, mode, 1024);
        let env = metrics::envelope_db(&out, 2048, 512);
        // Skip the ramps at either end.
        let a = env.len() / 6;
        let b = env.len() - env.len() / 6;
        let m = env[a..b].iter().sum::<f64>() / (b - a) as f64;
        let rms =
            (env[a..b].iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (b - a) as f64).sqrt();
        assert!(
            rms < 0.5,
            "{} let a steady tone breathe {rms:.3} dB rms",
            mode.label()
        );
    }
}

/// One search offset, one peak partition, one transient decision - applied to
/// every channel. The three fixtures here fail differently if any of that is
/// decided per channel.
#[test]
fn stereo_relationships_survive_the_engine() {
    let mono = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 1);
    for mode in [EngineMode::Polyphonic, EngineMode::Hybrid] {
        let identical = fixtures::harmonics(RATE as usize, RATE, 220.0, 8, 2);
        let (out, _) = render(&identical, 1.4, 3.0, mode, 1024);
        assert_eq!(out.channel(0), out.channel(1), "{} split an identical pair", mode.label());

        let inverted = fixtures::inverted_stereo(&mono);
        let (out, _) = render(&inverted, 1.4, 3.0, mode, 1024);
        let corr = metrics::channel_correlation(&out).unwrap();
        assert!(corr < -0.95, "{} lost the inversion: {corr:+.4}", mode.label());

        // A fixed inter-microphone delay is the stereo image. Stretching must
        // not move it; transposing scales it with the resampling, which is a
        // property of stretch-then-resample pitch shifting, not a per-channel
        // decision, so it is checked without a transpose.
        let hits = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
        let delayed = fixtures::delayed_stereo(&hits, 24);
        let (out, _) = render(&delayed, 1.4, 0.0, mode, 1024);
        let lag = metrics::interchannel_lag(&out, RATE as usize / 2, 16384, 200).unwrap();
        assert!(
            (lag - 24).abs() <= 2,
            "{} moved the microphone delay from 24 to {lag} frames",
            mode.label()
        );
    }
}


/// Render `src` with a given attack-protect setting.
fn render_protect(
    src: &AudioBuffer,
    mode: EngineMode,
    protect_s: f64,
) -> (AudioBuffer, solfege::RenderPlan) {
    let id = SourceIdentity::of(src, RATE);
    let mut doc = EditDocument::constant(id, 1.0, 2.0, mode);
    doc.quality = QualityProfile::Offline;
    let a = analyze(src, RATE, "protect", &AnalysisSettings::default());
    let opts = CompileOptions { protect_seconds: protect_s, ..CompileOptions::default() };
    let plan = compile(&doc, Some(&a), &opts).expect("plan compiles");
    let (out, _) = render_offline(&plan, src, 1024).expect("render succeeds");
    (out, plan)
}

/// Energy of an envelope slice, in linear units.
fn env_energy(env: &[f64], from: usize, to: usize) -> f64 {
    env[from.min(env.len())..to.min(env.len())]
        .iter()
        .map(|v| 10f64.powf(v / 10.0))
        .sum()
}

/// Attack Protect is a statement about phase, never about level.
///
/// It decides how the engine treats a transient - where it re-anchors, how
/// hard it locks - and a correct implementation never multiplies the output by
/// anything. The failure this guards against is real and was audible: a hard
/// phase reset disagrees with the three overlapping frames already written
/// around it, the overlap-add cancels, and the level drops at every hit, which
/// sounds exactly like a gate or a sidechain even though no gain was applied.
///
/// So: same material, protection off and on, and the energy around each
/// transient has to survive. The window is -50 to +150 ms because protection is
/// *allowed* to move an attack by a few milliseconds; it is not allowed to make
/// one quieter.
#[test]
fn attack_protect_does_not_duck_the_level() {
    let n = RATE as usize * 2;
    let cases: Vec<(&str, AudioBuffer)> = vec![
        ("drums", fixtures::drum_hits(n, RATE, RATE as usize / 4, 2)),
        ("bass+drums", fixtures::bass_under_hits(n, RATE, 62.0, RATE as usize / 4, 2)),
        ("mixed", fixtures::mixed(n, RATE, 2)),
    ];
    for mode in [EngineMode::Polyphonic, EngineMode::Hybrid] {
        for (name, src) in &cases {
            let (off, _) = render_protect(src, mode, 0.0);
            let (on, _) = render_protect(src, mode, 0.012);

            let win = 512usize;
            let hop = 128usize;
            let a = metrics::envelope_db(&off, win, hop);
            let b = metrics::envelope_db(&on, win, hop);
            let len = a.len().min(b.len());
            let skip = RATE as usize / 4 / hop;
            assert!(len > skip * 2 + 8, "{name}: too little material");

            let total = 10.0
                * (env_energy(&b, skip, len - skip) / env_energy(&a, skip, len - skip).max(1e-12))
                    .log10();
            assert!(
                total > -0.6,
                "{} on {name}: protection cost {total:.2} dB of total energy",
                mode.label()
            );

            let pre = (RATE as usize / 20) / hop;
            let post = (RATE as usize * 3 / 20) / hop;
            for h in metrics::peak_positions(&off, 0.45, RATE as usize / 8) {
                let c = h / hop;
                if c < pre + skip || c + post + skip >= len {
                    continue;
                }
                let ea = env_energy(&a, c - pre, c + post);
                if ea <= 1e-9 {
                    continue;
                }
                let d = 10.0 * (env_energy(&b, c - pre, c + post) / ea).log10();
                assert!(
                    d > -2.0,
                    "{} on {name}: the hit at {h} lost {d:.2} dB with protection on",
                    mode.label()
                );
            }

            let peak = 20.0 * ((on.peak().max(1e-9) / off.peak().max(1e-9)) as f64).log10();
            assert!(
                peak > -1.5,
                "{} on {name}: peak fell {peak:.2} dB with protection on",
                mode.label()
            );
        }
    }
}

/// Zero milliseconds of Attack Protect means the mechanism is off, not that the
/// protected window is short.
///
/// The engines also detect transients themselves, so "no protection windows"
/// and "no transient handling" are different states, and the control has to
/// select the second one or there is no setting that leaves the signal alone.
#[test]
fn zero_attack_protect_is_a_real_bypass() {
    let src = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 2);
    let (_, off) = render_protect(&src, EngineMode::Polyphonic, 0.0);
    assert!(
        !off.cfg.transient_protect,
        "protect 0 must switch transient handling off, not just empty the windows"
    );
    assert!(off.cfg.protections.is_empty());

    let (_, on) = render_protect(&src, EngineMode::Polyphonic, 0.012);
    assert!(on.cfg.transient_protect);
    assert!(
        !on.cfg.protections.is_empty(),
        "this fixture has onsets; protection should have found them"
    );
}

/// Nothing in the protected path scales the output.
///
/// A stronger statement than the level test: with the *same* protection
/// setting, rendering twice must be bit-identical, and turning protection on
/// must change the signal without changing its scale. If some hidden envelope
/// were multiplying the output, the ratio between the two renders would be a
/// smooth curve rather than the phase difference it should be.
#[test]
fn attack_protect_changes_phase_not_scale() {
    let src = fixtures::mixed(RATE as usize, RATE, 2);
    let (a1, _) = render_protect(&src, EngineMode::Polyphonic, 0.012);
    let (a2, _) = render_protect(&src, EngineMode::Polyphonic, 0.012);
    assert!(metrics::is_bit_identical(&a1, &a2), "protected render is not deterministic");

    let (off, _) = render_protect(&src, EngineMode::Polyphonic, 0.0);
    assert!(!metrics::is_bit_identical(&off, &a1), "protection did nothing at all");

    // Long-window RMS is a scale measurement; it must match closely even though
    // the two differ sample by sample.
    let ra = metrics::level(&a1);
    let rb = metrics::level(&off);
    assert!(
        (ra.rms_dbfs - rb.rms_dbfs).abs() < 0.6,
        "protection moved the overall RMS by {:.2} dB",
        ra.rms_dbfs - rb.rms_dbfs
    );
}

/// Seeking anywhere, with a transpose, has to keep producing.
///
/// `reset_to_zero_matches_a_fresh_prepare` only covers output frame zero, and
/// zero is the one seek where `output_frame * p` rounds exactly - which is why
/// it missed a deadlock in the transposition stage that fired on roughly half
/// of all other positions. The intermediate ring was labelled a kernel-context
/// earlier than the first frame the inner engine would ever produce, so when
/// the rounding went the other way the resampler's readiness test could never
/// be met, the ring filled, the inner engine stopped being called, and the
/// stage produced silence for ever. Live, that was every other slider move.
#[test]
fn seeking_with_a_transpose_keeps_producing() {
    let src = fixtures::mixed(RATE as usize * 2, RATE, 2);
    for mode in [EngineMode::Polyphonic, EngineMode::Hybrid, EngineMode::Monophonic] {
        for semis in [2.0f64, 3.0, -5.0] {
            let id = SourceIdentity::of(&src, RATE);
            let mut doc = EditDocument::constant(id, 1.3, semis, mode);
            doc.quality = QualityProfile::Offline;
            let a = analyze(&src, RATE, "seek", &AnalysisSettings::default());
            let plan = compile(&doc, Some(&a), &CompileOptions::default()).unwrap();
            let total = plan.output_frames();

            // Several positions, so an off-by-rounding cannot hide in one.
            for num in [1u64, 3, 7, 11, 17] {
                let target = total * num / 32;
                let mut engine = build_engine(&plan).unwrap();
                engine.reset(&PreparedSeek { output_frame: target, preroll: 0 });

                let mut in_pos = engine.input_position() as usize;
                let block = 1024usize;
                let mut scratch = vec![vec![0.0f32; block]; plan.cfg.channels];
                let mut produced = 0usize;
                let mut calls = 0;
                while produced == 0 && calls < 200 {
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
                     ({calls} calls, {in_pos} input frames consumed)",
                    mode.label()
                );
            }
        }
    }
}

/// Percussive has to do both halves of its job, and they pull against each
/// other.
///
/// Anchoring a transient and restarting the waveform are different operations,
/// and the first version of this engine did the second while trying to do the
/// first: it read every slice from that slice's own start, which places every
/// attack perfectly and throws away the phase of everything sustaining
/// underneath. A 55 Hz note came back wandering 113 cents and a reverb tail
/// 6 dB rougher than it went in.
///
/// So this checks the two together. A sustained tone has no onsets, nothing
/// about it is anchored, and its pitch has to hold; a drum track has nothing
/// but onsets, and they have to land where the map put them.
#[test]
fn percussive_anchors_transients_without_restarting_phase() {
    let n = RATE as usize * 2;

    // --- the phase timeline continues -----------------------------------
    for hz in [55.0f64, 82.5, 220.0] {
        let src = fixtures::harmonics(n, RATE, hz, 6, 2);
        let (out, _) = render(&src, 1.4, 0.0, EngineMode::Percussive, 1024);
        let mut cents: Vec<f64> = Vec::new();
        let mut at = RATE as usize / 4;
        while at + 8192 + RATE as usize / 4 < out.frames() {
            if let Some(f) = metrics::dominant_hz(&out, RATE, at, 8192) {
                cents.push(metrics::pitch_error_cents(f, hz));
            }
            at += 4096;
        }
        assert!(cents.len() > 4, "not enough steady material at {hz} Hz");
        let m = cents.iter().sum::<f64>() / cents.len() as f64;
        let rms =
            (cents.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / cents.len() as f64).sqrt();
        assert!(
            rms < 3.0,
            "percussive restarted the phase of a {hz} Hz tone: {rms:.2} cents rms"
        );
    }

    // --- the transients stay anchored -------------------------------------
    let src = fixtures::impulse_train(n, RATE as usize / 4, 2, 0);
    let a = analyze(&src, RATE, "perc", &AnalysisSettings::default());
    let id = SourceIdentity::of(&src, RATE);
    let mut doc = EditDocument::constant(id, 1.4, 0.0, EngineMode::Percussive);
    doc.quality = QualityProfile::Offline;
    let plan = compile(&doc, Some(&a), &CompileOptions::default()).unwrap();
    let (out, _) = render_offline(&plan, &src, 1024).unwrap();

    let src_peaks = metrics::peak_positions(&src, 0.5, RATE as usize / 8);
    let out_peaks = metrics::peak_positions(&out, 0.5, RATE as usize / 8);
    assert!(
        out_peaks.len() + 1 >= src_peaks.len(),
        "percussive lost hits: {} in, {} out",
        src_peaks.len(),
        out_peaks.len()
    );
    for (i, p) in out_peaks.iter().enumerate() {
        let Some(sp) = src_peaks.get(i) else { break };
        let want = plan.cfg.map.forward(*sp as f64);
        let err_ms = (*p as f64 - want).abs() * 1000.0 / RATE as f64;
        assert!(
            err_ms < 3.0,
            "hit {i} landed {err_ms:.2} ms from its anchor"
        );
    }
}

#[test]
fn tape_pitch_follows_the_ratio() {
    // Tape at alpha 2 halves the read rate: an octave down.
    let src = fixtures::sine(RATE as usize, RATE, 440.0, 0.5, 1);
    let (out, _) = render(&src, 2.0, 0.0, EngineMode::Tape, 1024);
    let hz = metrics::dominant_hz(&out, RATE, RATE as usize / 2, 8192).unwrap();
    let cents = metrics::pitch_error_cents(hz, 220.0);
    assert!(cents.abs() < 5.0, "tape produced {hz:.2} Hz ({cents:+.2} cents from 220)");
}

#[test]
fn stretching_alone_does_not_move_the_pitch() {
    let src = fixtures::sine(RATE as usize, RATE, 440.0, 0.5, 1);
    for mode in [EngineMode::Monophonic, EngineMode::Polyphonic] {
        for alpha in [0.6, 1.4, 2.0] {
            let (out, _) = render(&src, alpha, 0.0, mode, 1024);
            let hz = metrics::dominant_hz(&out, RATE, out.frames() / 3, 8192).unwrap();
            let cents = metrics::pitch_error_cents(hz, 440.0);
            assert!(
                cents.abs() < 10.0,
                "{} at alpha {alpha} moved the pitch by {cents:+.2} cents",
                mode.label()
            );
        }
    }
}

#[test]
fn transposition_hits_the_requested_interval() {
    let src = fixtures::sine(RATE as usize, RATE, 440.0, 0.5, 1);
    for semis in [-12.0, -7.0, -1.0, 1.0, 7.0, 12.0] {
        let (out, _) = render(&src, 1.0, semis, EngineMode::Polyphonic, 1024);
        let expected = 440.0 * 2f64.powf(semis / 12.0);
        let hz = metrics::dominant_hz(&out, RATE, out.frames() / 3, 8192).unwrap();
        let cents = metrics::pitch_error_cents(hz, expected);
        assert!(cents.abs() < 10.0, "{semis:+} semitones was off by {cents:+.2} cents");
    }
}

#[test]
fn identical_channels_stay_identical_and_inverted_stay_inverted() {
    let mono = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 1);
    let identical = fixtures::harmonics(RATE as usize / 2, RATE, 220.0, 6, 2);
    let inverted = fixtures::inverted_stereo(&mono);
    for mode in [EngineMode::Monophonic, EngineMode::Polyphonic, EngineMode::Hybrid] {
        let (out, _) = render(&identical, 1.6, 0.0, mode, 1024);
        assert_eq!(
            out.channel(0),
            out.channel(1),
            "{} split an identical pair",
            mode.label()
        );
        let (out, _) = render(&inverted, 1.6, 0.0, mode, 1024);
        let corr = metrics::channel_correlation(&out).unwrap();
        assert!(corr < -0.95, "{} lost the inversion: corr {corr:+.4}", mode.label());
    }
}

#[test]
fn a_hard_anchor_lands_where_it_was_put() {
    let n = RATE as usize;
    let src = fixtures::impulse_train(n, RATE as usize / 4, 1, 0);
    let mut doc = doc_for(&src, 1.0, 0.0, EngineMode::Percussive);
    let mid_src = RATE as u64 / 2;
    let mid_out = mid_src + RATE as u64 / 20;
    doc.anchors = vec![
        WarpAnchor::endpoint(0, 0),
        WarpAnchor::user(mid_src, mid_out),
        WarpAnchor::endpoint(n as u64, n as u64),
    ];
    let a = analyze(&src, RATE, "test", &AnalysisSettings::default());
    let plan = compile(&doc, Some(&a), &CompileOptions::default()).unwrap();
    let (out, _) = render_offline(&plan, &src, 1024).unwrap();
    let peaks = metrics::peak_positions(&out, 0.4, RATE as usize / 16);
    let nearest = peaks
        .iter()
        .map(|p| (*p as i64 - mid_out as i64).abs())
        .min()
        .expect("some impulses survived");
    let ms = nearest as f64 * 1000.0 / RATE as f64;
    assert!(ms <= 1.0, "nearest impulse was {ms:.3} ms from the anchor");
}

#[test]
fn compiler_rejects_before_it_renders() {
    let src = fixtures::harmonics(RATE as usize / 4, RATE, 220.0, 6, 1);

    // Tape cannot also transpose.
    let doc = doc_for(&src, 1.5, 3.0, EngineMode::Tape);
    assert!(matches!(
        compile(&doc, None, &CompileOptions::default()),
        Err(PlanError::TapePitchConflict { .. })
    ));

    // alpha 4 x pitch 2 is an internal 8x even though each control is legal.
    let doc = doc_for(&src, 4.0, 12.0, EngineMode::Monophonic);
    assert!(matches!(
        compile(&doc, None, &CompileOptions::default()),
        Err(PlanError::InternalRatio { internal, .. }) if (internal - 8.0).abs() < 1e-9
    ));

    // Formant control on an engine with no formant path.
    let mut doc = doc_for(&src, 1.2, 2.0, EngineMode::Monophonic);
    doc.formant = FormantPolicy::Preserve;
    assert!(matches!(
        compile(&doc, None, &CompileOptions::default()),
        Err(PlanError::FormantUnsupported { .. })
    ));
    // The same request is accepted by the polyphonic path.
    doc.mode = EngineMode::Polyphonic;
    assert!(compile(&doc, None, &CompileOptions::default()).is_ok());
}

#[test]
fn overlapping_protections_merge_before_the_feasibility_check() {
    use solfege::engines::ProtectWindow;
    let merged = merge_protections(vec![
        ProtectWindow { start: 100, end: 200 },
        ProtectWindow { start: 150, end: 260 },
        ProtectWindow { start: 900, end: 1000 },
    ]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0], ProtectWindow { start: 100, end: 260 });
    // Counted unmerged, these two would total 210 frames instead of 160.
    assert_eq!(merged[0].len(), 160);
}

#[test]
fn sustain_ratio_matches_the_documented_example() {
    // dsp.md sec.6: 100 -> 150 ms with a 10 ms attack gives 140/90.
    let r = sustain_ratio(100, 150, 10).unwrap();
    assert!((r - 140.0 / 90.0).abs() < 1e-12);
    // Infeasible cases return None rather than a negative ratio.
    assert!(sustain_ratio(100, 10, 10).is_none());
    assert!(sustain_ratio(10, 100, 10).is_none());
}

#[test]
fn infeasible_protection_is_a_conflict_not_a_dropped_hit() {
    let n = RATE as usize;
    let src = fixtures::drum_hits(n, RATE, RATE as usize / 8, 1);
    let a = analyze(&src, RATE, "test", &AnalysisSettings::default());
    assert!(!a.onsets.is_empty(), "fixture must have detectable onsets");
    // Squeeze to 0.3x while asking to protect 300 ms per attack: the protected
    // frames alone outlast the whole output segment.
    let doc = doc_for(&src, 0.3, 0.0, EngineMode::Percussive);
    let opts = CompileOptions { protect_seconds: 0.30, ..CompileOptions::default() };
    match compile(&doc, Some(&a), &opts) {
        Err(PlanError::ConstraintConflict { protected_frames, output_frames, .. }) => {
            assert!(protected_frames >= output_frames);
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("an impossible protection budget was accepted"),
    }
}

#[test]
fn a_group_with_mismatched_members_is_rejected() {
    use solfege::plan::check_group;
    let a_src = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
    let b_src = fixtures::drum_hits(RATE as usize / 2, RATE, RATE as usize / 4, 1);
    let a = doc_for(&a_src, 1.5, 0.0, EngineMode::Percussive);
    let b = doc_for(&b_src, 1.5, 0.0, EngineMode::Percussive);
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
fn auto_records_the_mode_it_chose() {
    let drums = fixtures::drum_hits(RATE as usize, RATE, RATE as usize / 4, 1);
    let doc = doc_for(&drums, 1.5, 0.0, EngineMode::Auto);
    let a = analyze(&drums, RATE, "test", &AnalysisSettings::default());
    let plan = compile(&doc, Some(&a), &CompileOptions::default()).unwrap();
    assert_ne!(plan.mode, EngineMode::Auto);
    assert!(plan.auto_reason.is_some(), "auto must say why it chose a mode");
}

#[test]
fn the_render_is_not_normalised_behind_the_users_back() {
    let quiet = fixtures::sine(RATE as usize / 2, RATE, 440.0, 0.05, 1);
    let (out, _) = render(&quiet, 1.5, 0.0, EngineMode::Polyphonic, 1024);
    assert!(out.peak() < 0.2, "output was normalised up: peak {}", out.peak());
}
