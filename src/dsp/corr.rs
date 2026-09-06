//! Normalised cross-correlation search for WSOLA.
//!
//! The search runs once per frame and the winning offset is applied to every
//! channel, so L and R can never drift apart. Channel scores are summed with an
//! energy weight rather than taken from a downmix, because a downmix lets
//! out-of-phase channels cancel exactly where the alignment matters most
//! (dsp.md sec.4, requirement 1).
//!
//! The search is coarse-to-fine. An exhaustive scan is
//! `(2*max_shift+1) * overlap * channels` multiply-adds per frame, which at a
//! +-5 ms search and a 15 ms overlap is roughly 700k for a stereo frame — one
//! frame's worth of that lands inside a single audio callback and was measured
//! at 7.6 ms of a 10 ms budget on real material. Decimating the first pass and
//! refining at full resolution around its winner costs about a twentieth of
//! that for the same offset in the ordinary case.
//!
//! The source is reached through a closure so the caller can index its own ring
//! by absolute frame; nothing here allocates.

/// Energy floor below which correlation is meaningless and the nominal
/// position wins. Well under a 16-bit dither floor.
const ENERGY_FLOOR: f64 = 1e-9;

#[derive(Copy, Clone, Debug)]
pub struct SearchResult {
    /// Offset in frames relative to the nominal position.
    pub offset: i64,
    pub score: f64,
    /// The frame was too quiet to correlate; the nominal position was kept.
    pub silent: bool,
}

/// Normalised correlation of one candidate offset against the target.
#[inline]
fn score_at<S>(
    channels: usize,
    sample: &S,
    target: &[Vec<f32>],
    start: i64,
    overlap: usize,
    step: usize,
    target_energy: f64,
) -> f64
where
    S: Fn(usize, i64) -> f32,
{
    let mut num = 0.0f64;
    let mut energy = 0.0f64;
    for (c, t) in target.iter().enumerate().take(channels) {
        let mut i = 0usize;
        while i < overlap {
            let x = sample(c, start + i as i64) as f64;
            num += x * (t[i] as f64);
            energy += x * x;
            i += step;
        }
    }
    if energy > ENERGY_FLOOR {
        num / (energy.sqrt() * target_energy.sqrt())
    } else {
        0.0
    }
}

/// Best alignment offset in `-max_shift..=max_shift`.
///
/// * `sample(c, frame)` - absolute source read.
/// * `target[c][..overlap]` - the natural continuation the new frame should
///   join onto.
/// * `nominal` - where the map wants the frame to start.
/// * `decimate` - stride for the coarse pass; 1 means an exhaustive search.
/// * `allowed(shift)` - protected transients and buffer bounds are enforced
///   here, never by clamping the winner afterwards.
#[allow(clippy::too_many_arguments)]
pub fn best_offset<S, A>(
    channels: usize,
    sample: S,
    target: &[Vec<f32>],
    nominal: i64,
    overlap: usize,
    max_shift: i64,
    decimate: usize,
    mut allowed: A,
) -> SearchResult
where
    S: Fn(usize, i64) -> f32,
    A: FnMut(i64) -> bool,
{
    if overlap == 0 || max_shift == 0 {
        return SearchResult { offset: 0, score: 0.0, silent: overlap == 0 };
    }
    let mut target_energy = 0.0f64;
    for t in target.iter().take(channels) {
        for v in t.iter().take(overlap) {
            target_energy += (*v as f64) * (*v as f64);
        }
    }
    if target_energy < ENERGY_FLOOR {
        return SearchResult { offset: 0, score: 0.0, silent: true };
    }

    let step = decimate.max(1);
    // The coarse pass sums a strided subset, so its energies are ~1/step of the
    // full ones. Normalisation cancels the scale, which is why the two passes
    // stay comparable in shape even though only the fine pass is exact.
    let coarse_target: f64 = if step == 1 {
        target_energy
    } else {
        let mut e = 0.0f64;
        for t in target.iter().take(channels) {
            let mut i = 0usize;
            while i < overlap {
                e += (t[i] as f64) * (t[i] as f64);
                i += step;
            }
        }
        e.max(ENERGY_FLOOR)
    };

    let mut best_shift = 0i64;
    let mut best_score = f64::NEG_INFINITY;
    let mut found = false;
    let mut shift = -max_shift;
    while shift <= max_shift {
        if allowed(shift) {
            let s = score_at(
                channels,
                &sample,
                target,
                nominal + shift,
                overlap,
                step,
                coarse_target,
            );
            if s > best_score {
                best_score = s;
                best_shift = shift;
            }
            found = true;
        }
        shift += step as i64;
    }
    if !found {
        return SearchResult { offset: 0, score: 0.0, silent: false };
    }
    if step == 1 {
        return SearchResult { offset: best_shift, score: best_score, silent: false };
    }

    // Refine at full resolution inside the coarse winner's cell.
    let lo = (best_shift - step as i64 + 1).max(-max_shift);
    let hi = (best_shift + step as i64 - 1).min(max_shift);
    let mut fine_shift = best_shift;
    let mut fine_score = f64::NEG_INFINITY;
    for s in lo..=hi {
        if !allowed(s) {
            continue;
        }
        let v = score_at(channels, &sample, target, nominal + s, overlap, 1, target_energy);
        if v > fine_score {
            fine_score = v;
            fine_shift = s;
        }
    }
    if fine_score == f64::NEG_INFINITY {
        return SearchResult { offset: best_shift, score: best_score, silent: false };
    }
    SearchResult { offset: fine_shift, score: fine_score, silent: false }
}
