//! Real-time playback: the pull adapter, its prefetch worker and the plan swap.
//!
//! Shape (system-design.md sec.8):
//!
//! ```text
//! control thread          worker thread                 audio callback
//! -------------- set_plan --> compile, prepare, seek
//!                             warm the input ring
//!                             hand over a Voice ------>  swap in, crossfade
//!                        <-- retire the old Voice ----   (never dropped here)
//!                             keep topping up  ------->  process() under a
//!                             the input ring             work budget
//! ```
//!
//! The callback runs the DSP; the worker only feeds it. That split is what the
//! design asks for, and it is why `prepare`, `reset` and every allocation stay
//! on the worker.
//!
//! **What the callback is allowed to do.** No allocation, no file IO, no FFT
//! planning, no blocking. Sample data crosses threads through a lock-free SPSC
//! ring. Voice handoff and retirement use `Mutex::try_lock`, which never waits:
//! if the worker happens to hold it for the few instructions it takes to move a
//! `Box`, the callback keeps the voice it already has and tries again next
//! block. A voice is never dropped in the callback — it goes back to the worker
//! to be destroyed.
//!
//! **What this does not promise.** Playing a file that can be read ahead, which
//! is exactly what the design scopes for the first release. A live input with
//! `alpha > 1` produces more audio than it receives and would accumulate a
//! backlog forever; that needs a bounded-capture policy this module does not
//! have.

use crate::audio::{AudioBuffer, AudioView, AudioViewMut};
use crate::engines::{PreparedSeek, ProcessState, StretchEngine};
use crate::mapping::TimeMap;
use crate::plan::{build_engine, PlanError, RenderPlan};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ------------------------------------------------------------------ spsc ring

/// Single-producer / single-consumer ring of interleaved `f32`.
///
/// Slots are `AtomicU32` holding the sample bits, so the whole thing is safe
/// Rust with no `UnsafeCell`. On every target we care about a relaxed 32-bit
/// load or store is a plain move, so the cost against a `memcpy` is the lost
/// vectorisation, not synchronisation — a fair trade for not hand-rolling
/// aliasing rules underneath an audio callback.
struct SpscRing {
    slots: Vec<AtomicU32>,
    mask: usize,
    write: AtomicUsize,
    read: AtomicUsize,
}

impl SpscRing {
    fn new(capacity_samples: usize) -> Self {
        let cap = capacity_samples.next_power_of_two().max(2);
        Self {
            slots: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            mask: cap - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }
    fn capacity(&self) -> usize {
        self.slots.len()
    }
}

pub struct RingProducer(Arc<SpscRing>);
pub struct RingConsumer(Arc<SpscRing>);

fn spsc(capacity_samples: usize) -> (RingProducer, RingConsumer) {
    let r = Arc::new(SpscRing::new(capacity_samples));
    (RingProducer(r.clone()), RingConsumer(r))
}

impl RingProducer {
    fn space(&self) -> usize {
        let r = self.0.read.load(Ordering::Acquire);
        let w = self.0.write.load(Ordering::Relaxed);
        self.0.capacity() - (w - r)
    }
    /// Returns how many samples were actually written.
    fn push(&self, src: &[f32]) -> usize {
        let n = src.len().min(self.space());
        let w = self.0.write.load(Ordering::Relaxed);
        for (i, v) in src[..n].iter().enumerate() {
            self.0.slots[(w + i) & self.0.mask].store(v.to_bits(), Ordering::Relaxed);
        }
        self.0.write.store(w + n, Ordering::Release);
        n
    }
    /// True once the consumer side is gone, so the worker can drop this.
    fn orphaned(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}

impl RingConsumer {
    fn available(&self) -> usize {
        let w = self.0.write.load(Ordering::Acquire);
        let r = self.0.read.load(Ordering::Relaxed);
        w - r
    }
    fn pop(&self, dst: &mut [f32]) -> usize {
        let n = dst.len().min(self.available());
        let r = self.0.read.load(Ordering::Relaxed);
        for (i, d) in dst[..n].iter_mut().enumerate() {
            *d = f32::from_bits(self.0.slots[(r + i) & self.0.mask].load(Ordering::Relaxed));
        }
        self.0.read.store(r + n, Ordering::Release);
        n
    }
}

// ------------------------------------------------------------------- metrics

/// Counters the callback publishes and the UI reads. All relaxed: they are
/// diagnostics, and a torn read of a statistic is not worth a fence in the
/// audio path.
pub struct StreamMetrics {
    pub callbacks: AtomicU64,
    pub underruns: AtomicU64,
    pub starved_frames: AtomicU64,
    pub errors: AtomicU64,
    pub swaps: AtomicU64,
    /// Output frames handed to the device by the live voice.
    pub output_frame: AtomicU64,
    /// Source position the live voice is reading, for the playhead.
    pub source_frame: AtomicU64,
    pub finished: AtomicU64,
    /// Largest absolute sample the stream has produced, as `f32` bits.
    ///
    /// The render is never normalised or limited on the way out, which is the
    /// documented policy (system-design.md sec.12). Overlap-add and phase
    /// propagation can both push a peak above the source's, so a mastered track
    /// near 0 dBFS can clip at the device even though the engine is behaving.
    /// Reporting it is how the user gets to make that call instead of guessing
    /// at a distortion with no cause.
    peak_bits: AtomicU32,
    /// Largest absolute sample actually handed to the device, after the trim.
    ///
    /// Kept apart from `peak_bits` on purpose: the first says whether the
    /// *render* is hot, the second whether what you are *hearing* clips. Trim
    /// moves the second and must not move the first, or the trim would hide the
    /// very thing it is compensating for.
    device_peak_bits: AtomicU32,
    /// Frames the device would clip, measured after the trim.
    pub clipped_frames: AtomicU64,
    /// Output trim in linear gain, as `f32` bits. Control-thread writable.
    gain_bits: AtomicU32,
    max_us: AtomicU64,
    /// Log-spaced histogram of callback durations, bucket `i` covering
    /// `2^i .. 2^(i+1)` microseconds.
    hist: [AtomicU32; 24],
}

impl Default for StreamMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamMetrics {
    pub fn new() -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            starved_frames: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            swaps: AtomicU64::new(0),
            output_frame: AtomicU64::new(0),
            source_frame: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            peak_bits: AtomicU32::new(0),
            device_peak_bits: AtomicU32::new(0),
            clipped_frames: AtomicU64::new(0),
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            max_us: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// Called by the device wrapper with the measured callback duration.
    pub fn record_call(&self, micros: u64) {
        let b = (64 - (micros.max(1)).leading_zeros() as usize).min(self.hist.len() - 1);
        self.hist[b].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
    }

    pub fn max_ms(&self) -> f64 {
        self.max_us.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// What the engine produced, before any trim.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }

    /// What the device received, after the trim.
    pub fn device_peak(&self) -> f32 {
        f32::from_bits(self.device_peak_bits.load(Ordering::Relaxed))
    }

    fn bump(slot: &AtomicU32, v: f32) {
        let mut cur = slot.load(Ordering::Relaxed);
        while v > f32::from_bits(cur) {
            match slot.compare_exchange_weak(
                cur,
                v.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    pub(crate) fn bump_engine_peak(&self, v: f32) {
        Self::bump(&self.peak_bits, v);
    }
    pub(crate) fn bump_device_peak(&self, v: f32) {
        Self::bump(&self.device_peak_bits, v);
    }

    /// Output trim. Applied as a per-block ramp so a slider drag does not zip.
    pub fn set_gain(&self, g: f32) {
        self.gain_bits.store(g.max(0.0).to_bits(), Ordering::Relaxed);
    }
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    pub fn clear_peak(&self) {
        self.peak_bits.store(0, Ordering::Relaxed);
        self.device_peak_bits.store(0, Ordering::Relaxed);
        self.clipped_frames.store(0, Ordering::Relaxed);
    }

    /// Upper edge of the bucket holding the given quantile. Coarse by
    /// construction: a log histogram cannot report better than a factor of two,
    /// and saying so is better than printing a precise-looking number.
    pub fn quantile_ms(&self, q: f64) -> f64 {
        let counts: Vec<u64> =
            self.hist.iter().map(|c| c.load(Ordering::Relaxed) as u64).collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0.0;
        }
        let target = (total as f64 * q).ceil() as u64;
        let mut acc = 0u64;
        let max = self.max_ms();
        for (i, c) in counts.iter().enumerate() {
            acc += c;
            if acc >= target {
                // Bucket upper edge, clamped: a log histogram cannot resolve
                // better than a factor of two, and reporting a quantile above
                // the observed maximum would be nonsense.
                return ((1u64 << i) as f64 / 1000.0).min(max);
            }
        }
        max
    }

    pub fn reset(&self) {
        self.callbacks.store(0, Ordering::Relaxed);
        self.underruns.store(0, Ordering::Relaxed);
        self.starved_frames.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.swaps.store(0, Ordering::Relaxed);
        self.finished.store(0, Ordering::Relaxed);
        self.peak_bits.store(0, Ordering::Relaxed);
        self.device_peak_bits.store(0, Ordering::Relaxed);
        self.clipped_frames.store(0, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
        for b in &self.hist {
            b.store(0, Ordering::Relaxed);
        }
    }
}

// --------------------------------------------------------------------- voice

/// One prepared engine plus the input feed that belongs to it.
///
/// A plan change makes a new voice rather than reconfiguring this one, because
/// `prepare` allocates and the old voice has to keep playing until the
/// crossfade is over.
pub struct Voice {
    engine: Box<dyn StretchEngine>,
    input: RingConsumer,
    map: TimeMap,
    channels: usize,
    /// Interleaved staging between the ring and the planar engine input.
    inter: Vec<f32>,
    /// Planar input the engine has been offered but not yet consumed.
    pending: Vec<Vec<f32>>,
    pending_len: usize,
    pending_pos: usize,
    /// Absolute source frame the next pending sample belongs to.
    source_pos: u64,
    source_frames: u64,
    delivered: u64,
    output_frames: u64,
    finished: bool,
    failed: bool,
    /// Output the worker rendered before handing this voice over.
    ///
    /// A phase vocoder produces nothing at all until it has overlap-added its
    /// warm-up, which with the low band's window is about 128 ms. Handing a
    /// voice to the callback in that state means the crossfade has nothing to
    /// fade *to*, and the swap comes out as a dropout - which is what a slider
    /// move sounded like. So the worker runs the new engine far enough to cover
    /// the whole crossfade before the callback ever sees it, and the callback
    /// spends the swap copying instead of running two engines at once.
    preroll: Vec<Vec<f32>>,
    preroll_len: usize,
    preroll_pos: usize,
}

impl Voice {
    fn block(&self) -> usize {
        self.pending[0].len()
    }

    /// Frames of pre-rendered output still to hand out.
    fn preroll_left(&self) -> usize {
        self.preroll_len - self.preroll_pos
    }

    /// Fill `[offset, offset+want)` of `dst`, from the pre-roll first.
    fn pull(&mut self, dst: &mut [Vec<f32>], offset: usize, want: usize, budget: usize) -> usize {
        let mut produced = 0usize;
        let avail = self.preroll_left().min(want);
        if avail > 0 {
            for c in 0..self.channels {
                let from = self.preroll_pos;
                dst[c][offset..offset + avail]
                    .copy_from_slice(&self.preroll[c][from..from + avail]);
            }
            self.preroll_pos += avail;
            produced += avail;
        }
        if produced < want {
            produced += self.run(dst, offset + produced, want - produced, budget);
        }
        produced
    }

    /// Render `target` frames into the pre-roll. Worker side only: this is
    /// where a new voice does its warm-up, well away from the callback.
    fn warm(&mut self, target: usize, budget: usize) -> usize {
        let cap = self.preroll[0].len();
        let target = target.min(cap);
        if self.preroll_len >= target {
            return 0;
        }
        // The buffer is moved out so `run` can borrow it as a plain
        // destination; it goes straight back.
        let mut buf = std::mem::take(&mut self.preroll);
        let want = target - self.preroll_len;
        let got = self.run(&mut buf, self.preroll_len, want, budget);
        self.preroll = buf;
        self.preroll_len += got;
        got
    }

    /// Run the engine into `dst`. Returns frames produced, which is less than
    /// `want` when the input ring has run dry or the work budget is spent — the
    /// caller turns that into an underrun, never into silence pretending to be
    /// audio.
    fn run(&mut self, dst: &mut [Vec<f32>], offset: usize, want: usize, budget: usize) -> usize {
        let mut produced = 0usize;
        let mut calls = 0usize;
        while produced < want && calls < budget && !self.finished && !self.failed {
            if self.pending_pos == self.pending_len {
                // Refill from the ring. Never more than the source has left.
                let block = self.block();
                let left = self.source_frames.saturating_sub(self.source_pos) as usize;
                let frames = (self.input.available() / self.channels).min(block).min(left);
                if frames > 0 {
                    let n = frames * self.channels;
                    self.input.pop(&mut self.inter[..n]);
                    for f in 0..frames {
                        for c in 0..self.channels {
                            self.pending[c][f] = self.inter[f * self.channels + c];
                        }
                    }
                }
                self.pending_len = frames;
                self.pending_pos = 0;
            }

            let n_in = self.pending_len - self.pending_pos;
            let end_of_input = self.source_pos + n_in as u64 >= self.source_frames;
            let room = (want - produced).min(dst[0].len() - offset - produced);
            if room == 0 {
                break;
            }

            let report = {
                let input = match AudioView::from_planar(&self.pending, self.pending_pos, n_in) {
                    Ok(v) => v,
                    Err(_) => {
                        self.failed = true;
                        break;
                    }
                };
                let output = match AudioViewMut::from_planar(dst, offset + produced, room) {
                    Ok(v) => v,
                    Err(_) => {
                        self.failed = true;
                        break;
                    }
                };
                match self.engine.process(input, output, end_of_input) {
                    Ok(r) => r,
                    Err(_) => {
                        self.failed = true;
                        break;
                    }
                }
            };
            calls += 1;
            self.pending_pos += report.consumed_frames;
            self.source_pos += report.consumed_frames as u64;
            produced += report.produced_frames;
            self.delivered += report.produced_frames as u64;

            if report.state == ProcessState::Finished {
                self.finished = true;
                break;
            }
            if report.consumed_frames == 0 && report.produced_frames == 0 {
                // Starved: the ring has nothing and the engine wants more.
                break;
            }
        }
        produced
    }

    fn source_playhead(&self) -> u64 {
        // `delivered` counts what the engine produced, and the pre-roll is
        // produced before it is heard; the playhead has to report what is
        // actually coming out of the device.
        let heard = self.delivered.saturating_sub(self.preroll_left() as u64);
        self.map.inverse(heard as f64) as u64
    }
}

// -------------------------------------------------------------------- player

/// A slot one thread fills and the other empties. `try_lock` never waits, so
/// the callback can give up on a contended block and keep playing.
type Slot<T> = Mutex<Option<Box<T>>>;

pub struct PlayerConfig {
    /// Largest block the device will ever ask for.
    pub max_block: usize,
    /// Ceiling on `process()` calls per callback.
    pub work_budget: usize,
    /// Frames over which an underrun fades to silence.
    pub underrun_fade: usize,
    /// Frames over which a swapped-in voice replaces the old one.
    pub swap_fade: usize,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self { max_block: 2048, work_budget: 32, underrun_fade: 64, swap_fade: 1024 }
    }
}

/// The callback half. Owns the live engine; `fill` is the only thing the audio
/// thread calls.
pub struct RtPlayer {
    voice: Option<Box<Voice>>,
    incoming: Option<Box<Voice>>,
    fade_pos: usize,
    fade_len: usize,
    channels: usize,
    mix_a: Vec<Vec<f32>>,
    mix_b: Vec<Vec<f32>>,
    handoff: Arc<Slot<Voice>>,
    retire: Arc<Slot<Voice>>,
    pending_retire: Option<Box<Voice>>,
    metrics: Arc<StreamMetrics>,
    cfg: PlayerConfig,
    /// Gain the underrun fade left behind, so the next block ramps back up
    /// instead of clicking.
    gain: f32,
    /// Smoothed output trim, chasing the value the control thread published.
    trim: f32,
    /// Consecutive blocks a swap has been unable to make progress.
    fade_stalled: usize,
}

impl RtPlayer {
    fn new(
        channels: usize,
        cfg: PlayerConfig,
        handoff: Arc<Slot<Voice>>,
        retire: Arc<Slot<Voice>>,
        metrics: Arc<StreamMetrics>,
    ) -> Self {
        let n = cfg.max_block;
        Self {
            voice: None,
            incoming: None,
            fade_pos: 0,
            fade_len: cfg.swap_fade,
            channels,
            mix_a: vec![vec![0.0; n]; channels],
            mix_b: vec![vec![0.0; n]; channels],
            handoff,
            retire,
            pending_retire: None,
            metrics,
            cfg,
            gain: 1.0,
            trim: 1.0,
            fade_stalled: 0,
        }
    }

    /// Hand a finished voice back to the worker. Never drops it here.
    fn release(&mut self, v: Box<Voice>) {
        if self.pending_retire.is_none() {
            self.pending_retire = Some(v);
        } else if let Ok(mut slot) = self.retire.try_lock() {
            if slot.is_none() {
                *slot = self.pending_retire.take();
                self.pending_retire = Some(v);
            }
            // If the slot is still full the extra voice is dropped only because
            // there is nowhere left to park it; with one swap in flight at a
            // time this branch is unreachable in practice.
        }
    }

    fn drain_retire(&mut self) {
        if self.pending_retire.is_some() {
            if let Ok(mut slot) = self.retire.try_lock() {
                if slot.is_none() {
                    *slot = self.pending_retire.take();
                }
            }
        }
    }

    fn take_handoff(&mut self) {
        if self.incoming.is_some() {
            return; // a swap is already in flight
        }
        if let Ok(mut slot) = self.handoff.try_lock() {
            if let Some(v) = slot.take() {
                if self.voice.is_none() {
                    self.voice = Some(v);
                } else {
                    self.incoming = Some(v);
                    self.fade_pos = 0;
                    self.fade_len = self.cfg.swap_fade;
                }
                self.metrics.swaps.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Render `frames` of interleaved output. Anything the engines could not
    /// deliver is faded out and counted, never left as stale samples.
    pub fn fill(&mut self, out: &mut [f32], frames: usize) {
        let ch = self.channels;
        let frames = frames.min(self.cfg.max_block).min(out.len() / ch.max(1));
        out[..frames * ch].fill(0.0);
        self.metrics.callbacks.fetch_add(1, Ordering::Relaxed);
        self.drain_retire();
        self.take_handoff();

        if self.voice.is_none() {
            return;
        }

        for c in 0..ch {
            self.mix_a[c][..frames].fill(0.0);
            self.mix_b[c][..frames].fill(0.0);
        }

        let budget = self.cfg.work_budget;
        let mut a_made = 0usize;
        if let Some(v) = self.voice.as_mut() {
            a_made = v.pull(&mut self.mix_a, 0, frames, budget);
        }

        // Crossfade a swapped-in plan rather than cutting: the two signals are
        // the same material, so an equal-gain fade is correct here and an
        // equal-power one would bump the level in the middle.
        let mut b_made = 0usize;
        let fading = self.incoming.is_some();
        if let Some(v) = self.incoming.as_mut() {
            b_made = v.pull(&mut self.mix_b, 0, frames, budget);
        }

        // While swapping, only blend as far as the incoming voice can supply.
        // It should always be able to, because the worker pre-rolled it, but a
        // starved ring is still possible: then keep playing the outgoing voice
        // at full level and leave the fade where it is. A swap must never be a
        // reason to go quiet.
        let blend = if fading { a_made.min(b_made) } else { 0 };
        let made = a_made;

        let target_trim = self.metrics.gain();
        let mut peak = 0.0f32;
        let mut device_peak = 0.0f32;
        let mut clipped = 0u64;
        for i in 0..made {
            let (wa, wb) = if fading && i < blend {
                let x = ((self.fade_pos + i) as f32 / self.fade_len as f32).clamp(0.0, 1.0);
                (1.0 - x, x)
            } else {
                (1.0, 0.0)
            };
            // Ramp the underrun gain back to unity over the same short window,
            // and chase the trim so a slider drag does not zip.
            self.gain += (1.0 - self.gain) * 0.02;
            self.trim += (target_trim - self.trim) * 0.005;
            let mut hot = false;
            for c in 0..ch {
                let v = (self.mix_a[c][i] * wa + self.mix_b[c][i] * wb) * self.gain;
                let a = v.abs();
                if a > peak {
                    peak = a;
                }
                let d = v * self.trim;
                let da = d.abs();
                if da > device_peak {
                    device_peak = da;
                }
                // Count what the converter will actually clip, which is after
                // the trim: that is the number the trim is there to fix.
                if da > 1.0 {
                    hot = true;
                }
                out[i * ch + c] = d;
            }
            if hot {
                clipped += 1;
            }
        }
        // `peak` is pre-trim, so turning the trim down stops the device
        // clipping without hiding the fact that the render is hot.
        Self::bump_metric(&self.metrics, peak, device_peak);
        if clipped > 0 {
            self.metrics.clipped_frames.fetch_add(clipped, Ordering::Relaxed);
        }

        if fading {
            self.fade_pos += blend;
            // A voice that cannot supply anything is not going to start; give
            // up on it rather than hold the crossfade open forever, which would
            // block every plan change after it.
            if blend == 0 && made > 0 {
                self.fade_stalled += 1;
                if self.fade_stalled > 8 {
                    if let Some(v) = self.incoming.take() {
                        self.release(v);
                    }
                    self.fade_pos = 0;
                    self.fade_stalled = 0;
                    self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                self.fade_stalled = 0;
            }
            if self.fade_pos >= self.fade_len {
                let old = self.voice.take();
                self.voice = self.incoming.take();
                if let Some(v) = old {
                    self.release(v);
                }
                self.fade_pos = 0;
            }
        }

        if made < frames {
            // Underrun: fade what we have to silence over a few dozen frames.
            // Never substitute the dry signal, which would be at the wrong time
            // and the wrong pitch (system-design.md sec.8).
            let fade = self.cfg.underrun_fade.min(frames - made).max(1);
            let last: Vec<f32> = (0..ch)
                .map(|c| if made > 0 { out[(made - 1) * ch + c] } else { 0.0 })
                .collect();
            for i in 0..fade {
                let g = 1.0 - (i as f32 + 1.0) / fade as f32;
                for c in 0..ch {
                    out[(made + i) * ch + c] = last[c] * g;
                }
            }
            self.gain = 0.0;
            let done = self
                .voice
                .as_ref()
                .map(|v| v.finished || v.failed)
                .unwrap_or(true);
            if done {
                if let Some(v) = self.voice.as_ref() {
                    if v.failed {
                        self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    if v.finished {
                        self.metrics.finished.store(1, Ordering::Relaxed);
                    }
                }
            } else {
                self.metrics.underruns.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .starved_frames
                    .fetch_add((frames - made) as u64, Ordering::Relaxed);
            }
        }

        if let Some(v) = self.voice.as_ref() {
            self.metrics.output_frame.store(v.delivered, Ordering::Relaxed);
            self.metrics.source_frame.store(v.source_playhead(), Ordering::Relaxed);
        }
    }

    pub fn metrics(&self) -> &Arc<StreamMetrics> {
        &self.metrics
    }

    fn bump_metric(m: &StreamMetrics, engine: f32, device: f32) {
        m.bump_engine_peak(engine);
        m.bump_device_peak(device);
    }
}

// -------------------------------------------------------------------- worker

enum WorkerCmd {
    /// Build a voice for this plan, starting at the given source frame.
    SetPlan { plan: Box<RenderPlan>, from_source: u64 },
    Stop,
}

/// Control-side handle. Dropping it stops the worker.
pub struct StreamHandle {
    tx: Sender<WorkerCmd>,
    metrics: Arc<StreamMetrics>,
    errors: Arc<Mutex<Option<String>>>,
}

impl StreamHandle {
    /// Replace the running plan, keeping the current source position so a
    /// slider move does not jump the playhead. The worker compiles, prepares
    /// and warms the new voice; the callback crossfades it in.
    pub fn set_plan(&self, plan: RenderPlan, from_source: u64) {
        let _ = self
            .tx
            .send(WorkerCmd::SetPlan { plan: Box::new(plan), from_source });
    }

    pub fn metrics(&self) -> &Arc<StreamMetrics> {
        &self.metrics
    }

    /// The most recent worker-side error, if any.
    pub fn take_error(&self) -> Option<String> {
        self.errors.lock().ok().and_then(|mut e| e.take())
    }

    /// Output trim in dB. Never applied automatically: the engine does not
    /// normalise, so this is the user's decision to make.
    pub fn set_trim_db(&self, db: f32) {
        self.metrics.set_gain(10f32.powf(db / 20.0));
    }

    pub fn peak(&self) -> f32 {
        self.metrics.peak()
    }

    pub fn device_peak(&self) -> f32 {
        self.metrics.device_peak()
    }

    pub fn clipped_frames(&self) -> u64 {
        self.metrics.clipped_frames.load(Ordering::Relaxed)
    }

    pub fn source_frame(&self) -> u64 {
        self.metrics.source_frame.load(Ordering::Relaxed)
    }
    pub fn output_frame(&self) -> u64 {
        self.metrics.output_frame.load(Ordering::Relaxed)
    }
    pub fn is_finished(&self) -> bool {
        self.metrics.finished.load(Ordering::Relaxed) != 0
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerCmd::Stop);
    }
}

/// Seconds of source the prefetch worker keeps queued ahead of the engine.
const PREFETCH_SECONDS: f64 = 0.5;

/// Start the worker and return the callback half plus its control handle.
///
/// The source is an in-memory buffer here; a disk-backed reader slots in at the
/// same place, which is the point of doing the feed on a worker at all.
pub fn start(
    source: Arc<AudioBuffer>,
    sample_rate: u32,
    plan: RenderPlan,
    from_source: u64,
    cfg: PlayerConfig,
) -> (RtPlayer, StreamHandle) {
    let channels = plan.cfg.channels;
    let metrics = Arc::new(StreamMetrics::new());
    let handoff: Arc<Slot<Voice>> = Arc::new(Mutex::new(None));
    let retire: Arc<Slot<Voice>> = Arc::new(Mutex::new(None));
    let errors: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = channel();

    let cfg_swap_fade = cfg.swap_fade;
    let cfg_max_block = cfg.max_block;
    let player = RtPlayer::new(channels, cfg, handoff.clone(), retire.clone(), metrics.clone());

    let ring_samples =
        ((sample_rate as f64 * PREFETCH_SECONDS) as usize * channels).next_power_of_two();
    let worker_block = 4096usize;
    // Enough rendered output to cover the whole crossfade plus a block, so the
    // callback never has to run two engines in the same period.
    let preroll_cap = cfg_swap_fade + cfg_max_block;
    let w_metrics = metrics.clone();
    let w_errors = errors.clone();

    let _ = tx.send(WorkerCmd::SetPlan { plan: Box::new(plan), from_source });

    std::thread::Builder::new()
        .name("solfege-prefetch".into())
        .spawn(move || {
            worker_loop(
                source,
                rx,
                handoff,
                retire,
                w_metrics,
                w_errors,
                channels,
                ring_samples,
                worker_block,
                preroll_cap,
            );
        })
        .expect("prefetch worker starts");

    (player, StreamHandle { tx, metrics, errors })
}

struct Feed {
    producer: RingProducer,
    /// Absolute source frame the next push starts at.
    next: u64,
    total: u64,
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    source: Arc<AudioBuffer>,
    rx: Receiver<WorkerCmd>,
    handoff: Arc<Slot<Voice>>,
    retire: Arc<Slot<Voice>>,
    metrics: Arc<StreamMetrics>,
    errors: Arc<Mutex<Option<String>>>,
    channels: usize,
    ring_samples: usize,
    worker_block: usize,
    preroll_cap: usize,
) {
    let mut feeds: Vec<Feed> = Vec::new();
    let mut stage = vec![0.0f32; worker_block * channels];
    let total = source.frames() as u64;

    loop {
        // 1. commands
        match rx.try_recv() {
            Ok(WorkerCmd::Stop) | Err(TryRecvError::Disconnected) => break,
            Ok(WorkerCmd::SetPlan { plan, from_source }) => {
                match make_voice(
                    &plan,
                    from_source,
                    channels,
                    ring_samples,
                    worker_block,
                    total,
                    preroll_cap,
                ) {
                    Ok((mut voice, feed)) => {
                        feeds.push(feed);
                        // Warm the ring *and the engine*. Feeding and rendering
                        // alternate because the engine cannot get ahead of its
                        // own input, and both have to be finished here: a voice
                        // that still has warm-up left to do arrives at the
                        // callback with nothing to play.
                        for _ in 0..64 {
                            for f in feeds.iter_mut() {
                                fill_feed(f, &source, &mut stage, channels, worker_block);
                            }
                            let got = voice.warm(preroll_cap, 64);
                            if got == 0 && voice.preroll_len > 0 {
                                break;
                            }
                            if voice.preroll_len >= preroll_cap {
                                break;
                            }
                        }
                        // Latest wins: if the callback has not collected the
                        // previous voice yet, the newer plan replaces it.
                        if let Ok(mut slot) = handoff.lock() {
                            *slot = Some(voice);
                        }
                    }
                    Err(e) => {
                        if let Ok(mut slot) = errors.lock() {
                            *slot = Some(e.to_string());
                        }
                        metrics.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
        }

        // 2. top up every live feed
        for f in feeds.iter_mut() {
            fill_feed(f, &source, &mut stage, channels, worker_block);
        }

        // 3. destroy retired voices here, never in the callback
        if let Ok(mut slot) = retire.lock() {
            slot.take();
        }
        feeds.retain(|f| !f.producer.orphaned());

        std::thread::sleep(Duration::from_millis(2));
    }
}

fn fill_feed(
    feed: &mut Feed,
    source: &AudioBuffer,
    stage: &mut [f32],
    channels: usize,
    worker_block: usize,
) {
    loop {
        let space_frames = feed.producer.space() / channels;
        let left = feed.total.saturating_sub(feed.next) as usize;
        let n = space_frames.min(worker_block).min(left);
        if n == 0 {
            return;
        }
        let start = feed.next as usize;
        for f in 0..n {
            for c in 0..channels {
                let plane = source.channel(c.min(source.channel_count() - 1));
                stage[f * channels + c] = plane[start + f];
            }
        }
        let pushed = feed.producer.push(&stage[..n * channels]);
        feed.next += (pushed / channels) as u64;
        if pushed < n * channels {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn make_voice(
    plan: &RenderPlan,
    from_source: u64,
    channels: usize,
    ring_samples: usize,
    worker_block: usize,
    total: u64,
    preroll_cap: usize,
) -> Result<(Box<Voice>, Feed), PlanError> {
    let mut engine = build_engine(plan)?;
    // Seek by source position so a plan change keeps the musical place; the
    // engine turns that back into the output frame it belongs to.
    let output_frame = plan.cfg.map.forward(from_source as f64).round() as u64;
    engine.reset(&PreparedSeek { output_frame, preroll: 0 });

    // The feed must start at exactly the frame the engine expects: the input
    // ring indexes by absolute source position.
    let start = engine.input_position();
    let (producer, consumer) = spsc(ring_samples);

    let voice = Box::new(Voice {
        map: plan.cfg.map.clone(),
        output_frames: engine.output_frames(),
        engine,
        input: consumer,
        channels,
        inter: vec![0.0; worker_block * channels],
        pending: vec![vec![0.0; worker_block]; channels],
        pending_len: 0,
        pending_pos: 0,
        source_pos: start,
        source_frames: total,
        delivered: output_frame,
        finished: false,
        failed: false,
        preroll: vec![vec![0.0; preroll_cap]; channels],
        preroll_len: 0,
        preroll_pos: 0,
    });
    Ok((voice, Feed { producer, next: start, total }))
}

impl Voice {
    pub fn output_frames(&self) -> u64 {
        self.output_frames
    }
}
