//! The real-time path, driven without an audio device: the test plays the
//! part of the callback, the worker thread runs for real.

use solfege::audio::{AudioBuffer, SourceIdentity};
use solfege::document::{EditDocument, EngineMode, FormantPolicy};
use solfege::plan::{compile, CompileOptions};
use solfege::stream::{self, PlayerConfig};
use solfege::fixtures;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const RATE: u32 = 48_000;
const BLOCK: usize = 512;

fn plan_for(src: &AudioBuffer, alpha: f64, semis: f64, mode: EngineMode) -> solfege::RenderPlan {
    let id = SourceIdentity::of(src, RATE);
    let mut doc = EditDocument::constant(id, alpha, semis, mode);
    doc.formant = FormantPolicy::FollowPitch;
    compile(&doc, None, &CompileOptions { max_block: 2048, ..CompileOptions::default() }).unwrap()
}

/// Play `blocks` callbacks, calling `control(i)` before each; returns the
/// interleaved output.
fn play(
    src: &AudioBuffer,
    first: solfege::RenderPlan,
    blocks: usize,
    mut control: impl FnMut(usize, &stream::StreamHandle),
) -> (Vec<f32>, Arc<solfege::StreamMetrics>) {
    let ch = src.channel_count();
    let (mut player, handle) = stream::start(
        Arc::new(src.clone()),
        RATE,
        first,
        0,
        PlayerConfig { max_block: 2048, ..PlayerConfig::default() },
    );
    // Let the worker build and warm the first voice.
    std::thread::sleep(Duration::from_millis(300));
    let mut out = Vec::with_capacity(blocks * BLOCK * ch);
    let mut buf = vec![0.0f32; BLOCK * ch];
    for i in 0..blocks {
        control(i, &handle);
        player.fill(&mut buf, BLOCK);
        out.extend_from_slice(&buf);
        // Roughly a quarter of real time: the worker tops the ring up every
        // couple of milliseconds.
        std::thread::sleep(Duration::from_micros(2500));
    }
    let m = handle.metrics().clone();
    (out, m)
}

fn block_rms_db(x: &[f32], ch: usize, block: usize) -> Vec<f64> {
    x.chunks(block * ch)
        .map(|b| {
            let e = b.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / b.len() as f64;
            10.0 * e.max(1e-20).log10()
        })
        .collect()
}

#[test]
fn slider_moves_retarget_in_place_without_dropouts() {
    let src = fixtures::harmonics(RATE as usize * 8, RATE, 220.0, 8, 2);
    for mode in [EngineMode::ElasticPro, EngineMode::ElasticEfficient, EngineMode::Soloist] {
        let alphas = [1.3, 0.8, 1.6, 1.0, 0.7, 1.2];
        let pitches = [2.0, -3.0, 5.0, 0.0, -1.0, 3.0];
        let (out, m) = play(&src, plan_for(&src, 1.0, 0.0, mode), 240, |i, h| {
            if i > 0 && i % 30 == 0 && i <= 180 {
                let k = i / 30 - 1;
                let p = plan_for(&src, alphas[k], pitches[k], mode);
                h.set_plan(p, h.source_frame());
            }
        });
        let retargets = m.retargets.load(Ordering::Relaxed);
        let swaps = m.swaps.load(Ordering::Relaxed);
        let underruns = m.underruns.load(Ordering::Relaxed);
        assert!(retargets >= 5, "{}: only {retargets} retargets", mode.label());
        assert_eq!(swaps, 1, "{}: a slider move rebuilt the engine", mode.label());
        assert_eq!(underruns, 0, "{}: {underruns} underruns", mode.label());
        assert_eq!(m.errors.load(Ordering::Relaxed), 0, "{}: the voice failed", mode.label());
        // No block may drop out: a steady tone stays within a few dB of its
        // own median level through every change.
        let lv = block_rms_db(&out, 2, 256);
        let mut sorted = lv.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];
        assert!(median > -30.0, "{}: no signal (median {median:.1} dB)", mode.label());
        let worst = lv[8..].iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(
            worst > median - 4.0,
            "{}: a block fell to {worst:.1} dB against a median of {median:.1} dB",
            mode.label()
        );
        // The last change (block 180: +3 semitones) is what is heard at the end.
        let tail = AudioBuffer::from_interleaved(&out[200 * BLOCK * 2..], 2).unwrap();
        let hz = solfege::metrics::dominant_hz(&tail, RATE, 0, 8192).unwrap();
        let want = 220.0 * 2f64.powf(3.0 / 12.0);
        let cents = solfege::metrics::pitch_error_cents(hz, want);
        assert!(cents.abs() < 10.0, "{}: {hz:.1} Hz after retarget, want {want:.1}", mode.label());
    }
}

#[test]
fn a_mode_change_still_crossfades_to_a_new_engine() {
    let src = fixtures::harmonics(RATE as usize * 4, RATE, 220.0, 8, 2);
    let (_, m) = play(&src, plan_for(&src, 1.2, 0.0, EngineMode::ElasticPro), 90, |i, h| {
        if i == 40 {
            h.set_plan(plan_for(&src, 1.2, 0.0, EngineMode::Soloist), h.source_frame());
        }
    });
    assert_eq!(m.swaps.load(Ordering::Relaxed), 2);
    assert_eq!(m.retargets.load(Ordering::Relaxed), 0);
    assert_eq!(m.underruns.load(Ordering::Relaxed), 0);
    assert_eq!(m.errors.load(Ordering::Relaxed), 0);
}
