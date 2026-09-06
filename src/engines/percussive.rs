//! Percussive: anchored transients over a continuous waveform.
//!
//! Cut points come from the plan, not from this engine, so a multi-microphone
//! group can be given one set of cuts and stay aligned (system-design.md
//! sec.9). Attacks play at their original rate and land on their mapped
//! position; only the sustain between hits is stretched or compressed.
//!
//! The thing this engine has to get right, and the thing the first version got
//! wrong, is that *anchoring a transient and restarting the waveform are not
//! the same operation*. Reading each slice from its own start at its own output
//! position places every attack perfectly - measured attack rise 1.17 ms
//! against a source of 1.19, onset error 0.85 ms - and throws away the phase of
//! everything sustaining underneath. A bass note wandered 113 cents and a
//! reverb tail came back 6.07 dB rougher than it went in, because every slice
//! boundary and every loop seam was a fresh start for a signal that never
//! stopped.
//!
//! So the read position is a *cursor*, not a function of the output offset. It
//! advances one frame per output frame and only ever moves by a crossfade:
//!
//! * **At an attack** the cursor is pulled to the anchor, because that is the
//!   whole point. The move happens in the pre-attack region and finishes on the
//!   attack itself, so the transient still lands exactly where the map put it
//!   while the join happens where there is no transient to damage.
//! * **Filling a gap** the cursor jumps back by an amount chosen by waveform
//!   similarity rather than arithmetic, so the sustain continues in phase
//!   instead of restarting. A fixed jump is only in phase when it happens to be
//!   a whole number of periods, which for a bass note it almost never is.
//!
//! An aligned jump joins two pieces of the same waveform, so it crossfades at
//! equal gain; the anchor jump joins two unrelated points and crossfades at
//! equal power (system-design.md sec.11).

use super::core::{EngineCore, Stepper};
use super::{
    Capability, LatencyInfo, PrepareError, PreparedConfig, PreparedSeek, ProcessError,
    ProcessReport, StretchEngine,
};
use crate::audio::{AudioView, AudioViewMut};
use crate::dsp::corr::best_offset;

/// Longest slice the engine will keep in one piece. Longer spans are cut into
/// sub-slices so the input ring stays bounded; sub-slice cuts carry no attack.
const MAX_SLICE_SECONDS: f64 = 0.5;

/// Longest jump back a gap fill will make, in seconds. Longer repeats more
/// material and is more obvious; shorter runs out of sustain to reuse.
const MAX_LOOP_SECONDS: f64 = 0.12;
/// Waveform-similarity search half-width, in seconds.
///
/// It has to span a whole period of the lowest note the engine expects to see,
/// or an aligned jump cannot reach a phase-matching position at all: at 55 Hz a
/// period is 18 ms, and a +-6 ms search left the bass wandering 26 cents purely
/// because the match it wanted was out of reach.
const LOOP_SEARCH_SECONDS: f64 = 0.026;
/// Correlation window for that search, in seconds. Two periods of a low bass
/// note, so the match is on the waveform and not on a fragment of one.
const LOOP_OVERLAP_SECONDS: f64 = 0.030;

#[derive(Copy, Clone, Debug)]
struct Slice {
    src_start: u64,
    src_end: u64,
    out_start: u64,
    out_end: u64,
    /// Frames at the head that play unstretched and are excluded from the loop
    /// region.
    attack: u64,
}

impl Slice {
    fn src_len(&self) -> u64 {
        self.src_end - self.src_start
    }
}

pub struct PercussiveEngine {
    core: Option<EngineCore>,
    slices: Vec<Slice>,
    index: usize,
    xfade: usize,
    /// Absolute source frame the next output frame reads.
    read: i64,
    /// The cursor being faded in, while a move is in progress.
    read_in: i64,
    /// Frames of the current crossfade still to go; zero when not moving.
    fade: usize,
    fade_len: usize,
    /// Equal power for an unrelated join, equal gain for an aligned one.
    fade_power: bool,
    /// Preallocated correlation target for the gap-fill search.
    target: Vec<Vec<f32>>,
    overlap: usize,
    search: i64,
    max_loop: usize,
    /// How well the last gap fill matched, for the UI. Not a quality score.
    last_score: f64,
}

impl Default for PercussiveEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PercussiveEngine {
    pub fn new() -> Self {
        Self {
            core: None,
            slices: Vec::new(),
            index: 0,
            xfade: 0,
            read: 0,
            read_in: 0,
            fade: 0,
            fade_len: 0,
            fade_power: false,
            target: Vec::new(),
            overlap: 0,
            search: 0,
            max_loop: 0,
            last_score: 0.0,
        }
    }

    pub const CAPABILITY: Capability = Capability {
        name: "percussive",
        min_ratio: 0.25,
        max_ratio: 8.0,
        independent_pitch: false,
        formant_control: false,
        realtime_safe: true,
    };

    pub fn slice_count(&self) -> usize {
        self.slices.len()
    }

    /// Correlation score of the last gap fill: a health signal, not a
    /// measurement of quality.
    pub fn last_fill_score(&self) -> f64 {
        self.last_score
    }

    fn build_slices(cfg: &PreparedConfig) -> Vec<Slice> {
        let n = cfg.source_frames();
        if n == 0 {
            return Vec::new();
        }
        let max_slice = (cfg.sample_rate as f64 * MAX_SLICE_SECONDS) as u64;

        // Cut at every protected attack, then subdivide anything too long.
        let mut cuts: Vec<(u64, u64)> = vec![(0, 0)];
        for p in &cfg.protections {
            if p.start > 0 && p.start < n {
                cuts.push((p.start, p.len()));
            }
        }
        cuts.sort_by_key(|c| c.0);
        cuts.dedup_by_key(|c| c.0);

        let mut bounds: Vec<(u64, u64)> = Vec::new();
        for (i, &(start, attack)) in cuts.iter().enumerate() {
            let end = cuts.get(i + 1).map(|c| c.0).unwrap_or(n);
            let mut a = start;
            let mut first = true;
            while a < end {
                let b = (a + max_slice).min(end);
                bounds.push((a, if first { attack } else { 0 }));
                let _ = b;
                a += max_slice;
                first = false;
            }
        }
        bounds.push((n, 0));

        let mut slices = Vec::with_capacity(bounds.len());
        for i in 0..bounds.len() - 1 {
            let (src_start, attack) = bounds[i];
            let src_end = bounds[i + 1].0;
            let out_start = cfg.map.forward(src_start as f64).round() as u64;
            let out_end = cfg.map.forward(src_end as f64).round() as u64;
            if out_end <= out_start || src_end <= src_start {
                continue;
            }
            slices.push(Slice {
                src_start,
                src_end,
                out_start,
                out_end,
                attack: attack.min(src_end - src_start),
            });
        }
        // The map guarantees monotonicity, so the last slice ends at M.
        if let Some(last) = slices.last_mut() {
            last.out_end = cfg.output_frames();
        }
        slices
    }

}

impl Stepper for PercussiveEngine {
    fn core(&mut self) -> &mut EngineCore {
        self.core.as_mut().expect("prepared")
    }
    fn core_ref(&self) -> &EngineCore {
        self.core.as_ref().expect("prepared")
    }

    fn step(&mut self) -> bool {
        let core = self.core.as_mut().expect("prepared");
        if core.remaining_output() == 0 || self.index >= self.slices.len() {
            return false;
        }
        let s = self.slices[self.index];
        if core.synth_pos >= s.out_end {
            self.index += 1;
            return self.index < self.slices.len();
        }
        // A gap fill can reach back anywhere inside the slice, and the join to
        // the next attack reads just before it, which is still inside this one.
        if !core.source_ready(s.src_start as i64, s.src_end as i64) {
            return false;
        }

        let room = core.out.space().min(core.remaining_output() as usize);
        let want = (s.out_end - core.synth_pos) as usize;
        let n = room.min(want);
        if n == 0 {
            return false;
        }

        // Entering a slice for the first time, the cursor has to be *at* the
        // anchor. That only happens for the very first slice; every later one
        // is reached by the pre-attack crossfade below, which is what keeps the
        // waveform continuous across the boundary.
        if core.synth_pos == s.out_start && self.index == 0 && self.fade == 0 {
            self.read = s.src_start as i64;
        }

        let xfade = self.xfade.max(1);
        let has_next = self.index + 1 < self.slices.len();
        let boundary = s.out_end.saturating_sub(xfade as u64);

        for i in 0..n {
            let o = core.synth_pos + i as u64;

            // --- move the cursor to the next slice --------------------------
            //
            // Only a *real* attack is an anchor. A boundary that exists because
            // a long span was subdivided carries no transient, so pinning the
            // cursor there buys nothing and costs the waveform its phase; those
            // are joined by alignment like any other jump. Tonal material has
            // no onsets at all, so on that material every boundary is soft and
            // the phase simply continues.
            if has_next && self.fade == 0 && o >= boundary {
                let next = self.slices[self.index + 1];
                let len = (s.out_end - o) as usize;
                let landing = s.src_end as i64 - len as i64;
                if next.attack > 0 {
                    // Anchored: land exactly on the attack, joining two
                    // unrelated points, so equal power to keep the energy.
                    self.read_in = landing;
                    self.fade_power = true;
                } else {
                    let ring = &core.ring;
                    let ov = self.overlap;
                    for c in 0..core.cfg.channels {
                        for k in 0..ov {
                            self.target[c][k] = ring.at(c, self.read - ov as i64 + k as i64);
                        }
                    }
                    let res = best_offset(
                        core.cfg.channels,
                        |c, f| ring.at(c, f),
                        &self.target,
                        landing - ov as i64,
                        ov,
                        self.search,
                        4,
                        |_| true,
                    );
                    self.last_score = res.score;
                    self.read_in = landing + res.offset;
                    self.fade_power = false;
                }
                self.fade = len;
                self.fade_len = len;
            }

            // --- fill a gap by jumping back, in phase -----------------------
            if self.fade == 0 && self.read >= s.src_end as i64 {
                let lo = s.src_start as i64 + s.attack as i64;
                // Jump back only as far as the gap actually needs. On a
                // decaying tail the jump *is* a level step - going back 120 ms
                // of a 260 ms decay lands 4 dB louder - so the shortest jump
                // that fills the gap is also the smallest step. Several small
                // ones beat one large one.
                let still_needed = (s.out_end - o) as i64 + xfade as i64;
                let back = still_needed
                    .clamp(2 * self.overlap as i64, self.max_loop as i64);
                let nominal = (self.read - back).max(lo);
                if nominal + self.overlap as i64 <= self.read - 1 {
                    let ring = &core.ring;
                    let ov = self.overlap;
                    for c in 0..core.cfg.channels {
                        for k in 0..ov {
                            self.target[c][k] = ring.at(c, self.read - ov as i64 + k as i64);
                        }
                    }
                    // The candidate's *preceding* material has to match what
                    // was just played, so the search is over positions ending
                    // where the jump would land.
                    let res = best_offset(
                        core.cfg.channels,
                        |c, f| ring.at(c, f),
                        &self.target,
                        nominal - ov as i64,
                        ov,
                        self.search,
                        4,
                        |d| {
                            let cand = nominal + d;
                            cand >= lo && cand < self.read
                        },
                    );
                    self.last_score = res.score;
                    self.read_in = nominal + res.offset;
                } else {
                    self.read_in = lo;
                    self.last_score = 0.0;
                }
                self.fade = xfade;
                self.fade_len = xfade;
                // Aligned: the two sides are the same waveform, so equal gain.
                self.fade_power = false;
            }

            // --- emit -------------------------------------------------------
            if self.fade > 0 {
                let x = (self.fade_len - self.fade) as f32 / self.fade_len as f32;
                let (wa, wb) = if self.fade_power {
                    let a = std::f32::consts::FRAC_PI_2 * x;
                    (a.cos(), a.sin())
                } else {
                    (1.0 - x, x)
                };
                for c in 0..core.cfg.channels {
                    let v = core.ring.at(c, self.read) * wa + core.ring.at(c, self.read_in) * wb;
                    core.out.add(c, i, v);
                }
                self.read += 1;
                self.read_in += 1;
                self.fade -= 1;
                if self.fade == 0 {
                    self.read = self.read_in;
                }
            } else {
                for c in 0..core.cfg.channels {
                    core.out.add(c, i, core.ring.at(c, self.read));
                }
                self.read += 1;
            }
        }

        core.out.advance(n);
        core.synth_pos += n as u64;
        if core.synth_pos >= s.out_end {
            self.index += 1;
        }
        // Keep whatever either cursor might still reach back to.
        let keep = self
            .slices
            .get(self.index)
            .map(|s| s.src_start)
            .unwrap_or(core.cfg.source_frames())
            .min(self.read.max(0) as u64)
            .saturating_sub(self.max_loop as u64 + self.overlap as u64 + 64);
        core.ring.discard_before(keep);
        true
    }
}

impl StretchEngine for PercussiveEngine {
    fn capability(&self) -> Capability {
        Self::CAPABILITY
    }

    fn prepare(&mut self, cfg: &PreparedConfig) -> Result<(), PrepareError> {
        if !cfg.formant.is_identity() {
            return Err(PrepareError::FormantNotSupported);
        }
        let (lo, hi) = cfg.map.ratio_range();
        let cap = Self::CAPABILITY;
        if lo < cap.min_ratio || hi > cap.max_ratio {
            return Err(PrepareError::UnsupportedRatio {
                requested: if lo < cap.min_ratio { lo } else { hi },
                min: cap.min_ratio,
                max: cap.max_ratio,
            });
        }
        self.slices = Self::build_slices(cfg);
        self.index = 0;
        self.xfade = (cfg.sample_rate as f64 * 0.005).round() as usize;
        self.max_loop = (cfg.sample_rate as f64 * MAX_LOOP_SECONDS).round() as usize;
        self.search = (cfg.sample_rate as f64 * LOOP_SEARCH_SECONDS).round() as i64;
        self.overlap = (cfg.sample_rate as f64 * LOOP_OVERLAP_SECONDS).round() as usize;
        self.target = vec![vec![0.0; self.overlap]; cfg.channels];
        self.read = self.slices.first().map(|s| s.src_start as i64).unwrap_or(0);
        self.read_in = self.read;
        self.fade = 0;
        self.fade_len = 0;
        self.fade_power = false;
        self.last_score = 0.0;

        let max_slice = (cfg.sample_rate as f64 * MAX_SLICE_SECONDS) as usize;
        // The gap-fill search reaches back a loop plus a correlation window.
        let ring = max_slice + self.max_loop + self.overlap + cfg.max_block + 1024;
        let out_cap = (cfg.max_block * 2).max(8192);
        self.core = Some(EngineCore::new(cfg.clone(), ring, out_cap));
        Ok(())
    }

    fn reset(&mut self, position: &PreparedSeek) {
        let idx = self
            .slices
            .iter()
            .position(|s| position.output_frame < s.out_end)
            .unwrap_or(self.slices.len());
        self.index = idx;
        self.fade = 0;
        self.fade_len = 0;
        self.fade_power = false;
        self.last_score = 0.0;
        if let Some(core) = self.core.as_mut() {
            let s = self.slices.get(idx).copied();
            let start = s.map(|s| s.src_start).unwrap_or(0);
            // Seeking into the middle of a slice: the cursor is however far
            // into that slice the seek landed, which keeps the anchor in the
            // right place relative to the output.
            let into = s
                .map(|s| position.output_frame.saturating_sub(s.out_start))
                .unwrap_or(0);
            self.read = (start + into.min(s.map(|s| s.src_len()).unwrap_or(0))) as i64;
            self.read_in = self.read;
            core.reset_to(start.saturating_sub(self.max_loop as u64 + self.overlap as u64));
            core.synth_pos = position.output_frame;
            core.delivered = position.output_frame;
        }
    }

    fn process(
        &mut self,
        input: AudioView<'_>,
        output: AudioViewMut<'_>,
        end_of_input: bool,
    ) -> Result<ProcessReport, ProcessError> {
        self.drive(input, output, end_of_input)
    }

    fn latency(&self) -> LatencyInfo {
        let sr = self.core.as_ref().map(|c| c.cfg.sample_rate).unwrap_or(48_000) as f64;
        LatencyInfo {
            lookahead_input_frames: (MAX_SLICE_SECONDS * sr) as u64,
            startup_padding_input_frames: 0,
            presentation_delay_output_frames: 0,
            tail_output_frames: 0,
        }
    }

    fn output_frames(&self) -> u64 {
        self.core.as_ref().map(|c| c.total_output()).unwrap_or(0)
    }

    fn input_position(&self) -> u64 {
        self.core.as_ref().map(|c| c.ring.end()).unwrap_or(0)
    }
}
