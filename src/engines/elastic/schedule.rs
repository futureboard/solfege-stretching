//! Where each analysis frame is read from: the map, bent around transients.
//!
//! A phase vocoder smears a transient because the frames that contain it are
//! read `Ha` apart and written `Hs` apart: every frame puts its copy of the hit
//! at a slightly different output time. The copies line up again only if the
//! frames around the hit are read at the rate they are written - unity - and
//! that is what a *lock* does. Around each detected onset `o` the read
//! position follows the line `a(t) = o + (t - W(o))`, so the hit lands at
//! exactly `W(o)`, where the map says it belongs, and passes through the
//! kernel as if nothing was being stretched at all.
//!
//! The time the lock borrows is paid back on either side by ramps, over which
//! the read rate deviates from the map by at most a fixed factor. Everything is
//! a pure function of the map and the onsets, so it is repeatable and
//! independent of the caller's block size. A lock is only accepted if its
//! whole span - ramps included - sits inside one map segment, which keeps every
//! anchor, and both endpoints, exact.

use crate::mapping::TimeMap;
use crate::runtime::InputRing;
use std::collections::VecDeque;

#[derive(Copy, Clone, Debug)]
struct Lock {
    /// Source frame of the onset.
    onset: f64,
    /// Output frame it has to land on: `W(onset)`.
    at: f64,
    /// Half-width of the unity span, output frames.
    half: f64,
    /// Read-position offset from the map at the lock edges.
    dev: f64,
    /// Ramp length on each side, output frames.
    ramp: f64,
}

impl Lock {
    fn lo(&self) -> f64 {
        self.at - self.half - self.ramp
    }
    fn hi(&self) -> f64 {
        self.at + self.half + self.ramp
    }
}

/// `W^-1`, extended linearly past both ends with the edge segments' slope, so
/// frames that hang over the start or end of the render read where they
/// would if the map went on.
pub fn inverse_ext(map: &TimeMap, t: f64) -> f64 {
    let m = map.output_frames().as_f64();
    if map.is_empty() {
        return t;
    }
    if t < 0.0 {
        t / map.ratio_at_output(0.0)
    } else if t > m {
        map.source_frames().as_f64() + (t - m) / map.ratio_at_output(m)
    } else {
        map.inverse(t)
    }
}

/// `W`, extended the same way.
pub fn forward_ext(map: &TimeMap, s: f64) -> f64 {
    let n = map.source_frames().as_f64();
    if map.is_empty() {
        return s;
    }
    if s < 0.0 {
        s * map.ratio_at_source(0.0)
    } else if s > n {
        map.output_frames().as_f64() + (s - n) * map.ratio_at_source(n)
    } else {
        map.forward(s)
    }
}

pub struct Scheduler {
    hop: usize,
    /// Half-width of the unity span in the kernel's own timeline.
    half: f64,
    /// Read speed that counts as "unity" for the kernel, in source frames
    /// per output frame. With a transposition `p` the kernel runs `p` times
    /// faster than the output, so unity for it is `p`.
    unity: f64,
    rate_dev: f64,
    ramp_cap: f64,
    enabled: bool,
    locks: VecDeque<Lock>,
    /// End of the last accepted lock's span (output frames).
    last_end: f64,
    /// Output frame before which no new lock may start (set on reset).
    earliest: f64,
    /// Read-position correction left behind by a retarget, fading out.
    carry: f64,
    carry_at: f64,
    carry_len: f64,
    /// Every detected onset (source frame), accepted as a lock or not: the
    /// kernel treats frames around any of them as transient frames.
    onsets: VecDeque<f64>,
    /// Half-width of the window the kernel analyses, source frames.
    window_half: f64,
}

impl Scheduler {
    pub fn new(
        n: usize,
        hop: usize,
        sample_rate: u32,
        lock_span: f64,
        rate_dev: f64,
        enabled: bool,
    ) -> Self {
        Self {
            hop,
            half: lock_span * n as f64 / 2.0,
            unity: 1.0,
            rate_dev: rate_dev.max(1.05),
            ramp_cap: 0.35 * sample_rate as f64,
            enabled,
            locks: VecDeque::with_capacity(64),
            last_end: f64::NEG_INFINITY,
            earliest: f64::NEG_INFINITY,
            carry: 0.0,
            carry_at: 0.0,
            carry_len: 1.0,
            onsets: VecDeque::with_capacity(64),
            window_half: n as f64 / 2.0,
        }
    }

    /// Is an onset inside the analysis window centred on source frame `a`?
    pub fn transient_in(&self, a: i64) -> bool {
        let a = a as f64;
        self.onsets.iter().any(|&o| (o - a).abs() <= self.window_half)
    }

    /// Set what counts as unity for locks made from now on.
    pub fn set_unity(&mut self, q: f64) {
        self.unity = q.clamp(0.1, 10.0);
    }

    pub fn reset(&mut self) {
        self.onsets.clear();
        self.locks.clear();
        self.last_end = f64::NEG_INFINITY;
        self.earliest = f64::NEG_INFINITY;
        self.carry = 0.0;
    }

    /// Source frames the scheduler may need beyond the current frame centre,
    /// at read speed `speed`, in the worst case.
    pub fn lookahead_output(&self, speed: f64) -> usize {
        if !self.enabled {
            return 0;
        }
        // Unity may be as low as 1/4 (two octaves down), which widens the
        // lock in output frames four times.
        ((self.half * 4.0 + self.ramp_cap) * speed.max(1.0) + self.half * 4.0).ceil() as usize
    }

    /// Onsets must be known up to this source frame before the frame centred
    /// at `centre` can be scheduled.
    pub fn detect_horizon(&self, map: &TimeMap, t: f64) -> i64 {
        if !self.enabled {
            return i64::MIN;
        }
        inverse_ext(map, t + self.half / self.unity + self.ramp_cap).ceil() as i64 + 1
    }

    fn deviation(&self, t: f64) -> f64 {
        let mut d = 0.0;
        for l in &self.locks {
            let u = t - l.at;
            let edge = l.half;
            if u < -edge - l.ramp {
                break; // locks are sorted; nothing later applies
            }
            if u > edge + l.ramp {
                continue;
            }
            d = if u.abs() <= edge {
                u * l.dev / edge
            } else if u < 0.0 {
                -l.dev * (u + edge + l.ramp) / l.ramp
            } else {
                l.dev * (1.0 - (u - edge) / l.ramp)
            };
            break;
        }
        if self.carry != 0.0 {
            let x = ((t - self.carry_at) / self.carry_len).clamp(0.0, 1.0);
            d += self.carry * (1.0 - x);
        }
        d
    }

    /// Integer source frame to centre the analysis frame on, for a synthesis
    /// frame centred at output time `t`.
    pub fn position(&self, map: &TimeMap, t: f64) -> i64 {
        (inverse_ext(map, t) + self.deviation(t)).round() as i64
    }

    fn make_lock(&self, map: &TimeMap, onset: f64) -> Option<Lock> {
        let at = forward_ext(map, onset);
        let speed = 1.0 / map.ratio_at_source(onset.clamp(0.0, map.source_frames().as_f64()));
        let q = self.unity;
        let half = self.half / q;
        // Inside the lock the read position runs at `q` instead of `speed`.
        let dev = half * (q - speed);
        if dev.abs() < 1.0 {
            return None; // already unity: nothing to straighten
        }
        let slack = if q > speed {
            speed * (1.0 - 1.0 / self.rate_dev)
        } else {
            speed * (self.rate_dev - 1.0)
        };
        let ramp = (dev.abs() / slack).max(self.hop as f64);
        if ramp > self.ramp_cap - self.hop as f64 {
            return None;
        }
        let lo = at - half - ramp;
        let hi = at + half + ramp;
        let m = map.output_frames().as_f64();
        if lo < 0.0 || hi > m {
            return None;
        }
        // Whole span inside one segment: no anchor strictly inside.
        let anchors = map.anchors();
        let i = anchors.partition_point(|a| (a.output_frame as f64) <= lo);
        if i < anchors.len() && (anchors[i].output_frame as f64) < hi {
            return None;
        }
        Some(Lock { onset, at, half, dev, ramp })
    }

    /// Consider one detected onset. Onsets arrive in source order.
    pub fn offer_onset(&mut self, map: &TimeMap, onset: u64, centre: f64) {
        if !self.enabled {
            return;
        }
        while let Some(&o) = self.onsets.front() {
            if self.onsets.len() == self.onsets.capacity()
                || forward_ext(map, o) + 8.0 * self.window_half < centre
            {
                self.onsets.pop_front();
            } else {
                break;
            }
        }
        self.onsets.push_back(onset as f64);
        // Retire locks that are fully behind us.
        while let Some(l) = self.locks.front() {
            if l.hi() < centre - self.half * 4.0 {
                self.locks.pop_front();
            } else {
                break;
            }
        }
        let Some(lock) = self.make_lock(map, onset as f64) else {
            return;
        };
        if lock.lo() < self.last_end || lock.lo() < self.earliest {
            return;
        }
        if self.locks.len() == self.locks.capacity() {
            return; // bounded; a pathological onset density just loses locks
        }
        self.last_end = lock.hi();
        self.locks.push_back(lock);
    }

    /// Forbid locks whose span starts before output time `t` (after a seek,
    /// the onsets before the new position were never seen).
    pub fn start_at(&mut self, t: f64) {
        self.earliest = t;
    }

    /// Move to `new_map` without a jump in the read position. Returns the
    /// offset to add to every output-frame label.
    pub fn retarget(&mut self, new_map: &TimeMap, old_map: &TimeMap, t: f64) -> i64 {
        let s_now = inverse_ext(old_map, t) + self.deviation(t);
        let t_new = forward_ext(new_map, inverse_ext(old_map, t));
        let offset = (t_new - t).round() as i64;
        let t_new = t + offset as f64;

        // Rebuild locks under the new map.
        let old: [Option<Lock>; 64] = {
            let mut a = [None; 64];
            for (i, l) in self.locks.iter().enumerate().take(64) {
                a[i] = Some(*l);
            }
            a
        };
        self.locks.clear();
        self.last_end = f64::NEG_INFINITY;
        self.carry = 0.0;
        for l in old.iter().flatten() {
            if let Some(nl) = self.make_lock(new_map, l.onset) {
                if nl.lo() >= self.last_end && nl.lo() >= t_new {
                    self.last_end = nl.hi();
                    self.locks.push_back(nl);
                }
            }
        }
        let s_new = inverse_ext(new_map, t_new) + self.deviation(t_new);
        let carry = s_now - s_new;
        if carry.abs() > 0.5 {
            let speed = 1.0 / new_map.ratio_at_output(t_new.max(0.0)).max(1e-6);
            self.carry = carry;
            self.carry_at = t_new;
            self.carry_len = (carry.abs() / (speed * 0.5)).max(self.hop as f64);
        }
        offset
    }
}

/// A cheap, causal onset detector: energy of the first difference in short
/// blocks, against the running mean of the blocks before it.
pub struct OnsetDetector {
    channels: usize,
    block: usize,
    pos: u64,
    last: Vec<f32>,
    acc: f64,
    count: usize,
    best: f32,
    best_pos: u64,
    hist: [f64; 8],
    hist_n: usize,
    hist_i: usize,
    prev: f64,
    refractory: u64,
    last_onset: Option<u64>,
    found: VecDeque<u64>,
    floor: f64,
}

impl OnsetDetector {
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        let block = ((sample_rate as f64 * 0.0027).round() as usize).next_power_of_two().max(32);
        Self {
            channels,
            block,
            pos: 0,
            last: vec![0.0; channels],
            acc: 0.0,
            count: 0,
            best: 0.0,
            best_pos: 0,
            hist: [0.0; 8],
            hist_n: 0,
            hist_i: 0,
            prev: 0.0,
            refractory: (sample_rate as f64 * 0.05) as u64,
            last_onset: None,
            found: VecDeque::with_capacity(64),
            floor: block as f64 * channels as f64 * 1e-8,
        }
    }

    pub fn reset(&mut self, pos: u64) {
        self.pos = pos;
        self.last.fill(0.0);
        self.acc = 0.0;
        self.count = 0;
        self.best = 0.0;
        self.hist_n = 0;
        self.hist_i = 0;
        self.prev = 0.0;
        self.last_onset = None;
        self.found.clear();
    }

    /// Next source frame not yet scanned.
    pub fn position(&self) -> i64 {
        self.pos as i64
    }

    pub fn pop(&mut self) -> Option<u64> {
        self.found.pop_front()
    }

    pub fn scan(&mut self, ring: &InputRing, src_len: i64) {
        let from = self.pos.max(ring.start());
        let to = (ring.end() as i64).min(src_len).max(0) as u64;
        if from > self.pos {
            // The ring moved past us (never after a proper reset); skip.
            self.pos = from;
            self.count = 0;
            self.acc = 0.0;
        }
        let mut s = self.pos;
        while s < to {
            let mut e = 0.0f32;
            for c in 0..self.channels {
                let x = ring.at(c, s as i64);
                let d = x - self.last[c];
                self.last[c] = x;
                e += d * d;
            }
            self.acc += e as f64;
            if e > self.best {
                self.best = e;
                self.best_pos = s;
            }
            self.count += 1;
            s += 1;
            if self.count == self.block {
                self.close_block();
            }
        }
        self.pos = s;
    }

    fn close_block(&mut self) {
        let e = self.acc;
        let mean = if self.hist_n > 0 {
            self.hist[..self.hist_n].iter().sum::<f64>() / self.hist_n as f64
        } else {
            0.0
        };
        let onset = self.hist_n >= 4
            && e > 8.0 * mean + self.floor
            && e > 2.0 * self.prev
            && self.last_onset.map_or(true, |l| self.best_pos >= l + self.refractory);
        if onset {
            self.last_onset = Some(self.best_pos);
            if self.found.len() == self.found.capacity() {
                self.found.pop_front();
            }
            self.found.push_back(self.best_pos);
        }
        self.hist[self.hist_i] = e;
        self.hist_i = (self.hist_i + 1) % self.hist.len();
        self.hist_n = (self.hist_n + 1).min(self.hist.len());
        self.prev = e;
        self.acc = 0.0;
        self.count = 0;
        self.best = 0.0;
    }
}
