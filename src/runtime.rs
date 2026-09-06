//! Bounded, preallocated buffers for the process contract.
//!
//! Both rings are linear buffers with compaction rather than modular rings: the
//! engines want contiguous slices for correlation and FFT windows, and the
//! occasional memmove inside an already-allocated buffer is cheaper than the
//! per-sample index wrap it replaces. Neither type allocates after `new`
//! (system-design.md sec.8).

use crate::audio::{AudioView, AudioViewMut};

/// Input side: holds source frames by absolute source position.
pub struct InputRing {
    planes: Vec<Vec<f32>>,
    capacity: usize,
    /// Absolute source frame of `planes[c][0]`.
    start: u64,
    len: usize,
}

impl InputRing {
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self {
            planes: vec![vec![0.0; capacity]; channels],
            capacity,
            start: 0,
            len: 0,
        }
    }

    pub fn reset(&mut self, start: u64) {
        self.start = start;
        self.len = 0;
        for p in &mut self.planes {
            p.fill(0.0);
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    #[inline]
    pub fn start(&self) -> u64 {
        self.start
    }
    #[inline]
    pub fn end(&self) -> u64 {
        self.start + self.len as u64
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[inline]
    pub fn space(&self) -> usize {
        self.capacity - self.len
    }
    #[inline]
    pub fn plane(&self, c: usize) -> &[f32] {
        &self.planes[c][..self.len]
    }
    pub fn planes(&self) -> Vec<&[f32]> {
        self.planes.iter().map(|p| &p[..self.len]).collect()
    }

    /// Absolute frame read, zero outside what the ring holds. Callers check
    /// `holds()` first when the difference matters.
    #[inline]
    pub fn at(&self, c: usize, frame: i64) -> f32 {
        let i = frame - self.start as i64;
        if i < 0 || i as usize >= self.len {
            0.0
        } else {
            self.planes[c][i as usize]
        }
    }

    /// True when `[from, to)` is entirely inside the ring.
    #[inline]
    pub fn holds(&self, from: i64, to: i64) -> bool {
        from >= self.start as i64 && to <= self.end() as i64
    }

    /// Append from a caller buffer. Returns frames actually taken; the caller
    /// re-submits the rest (process contract item 3).
    pub fn push(&mut self, input: &AudioView<'_>, from: usize) -> usize {
        let avail = input.frames().saturating_sub(from);
        let n = avail.min(self.space());
        if n == 0 {
            return 0;
        }
        for c in 0..self.planes.len().min(input.channel_count()) {
            let src = &input.channel(c)[from..from + n];
            self.planes[c][self.len..self.len + n].copy_from_slice(src);
        }
        self.len += n;
        n
    }

    /// Drop everything before `frame`, freeing space at the front.
    pub fn discard_before(&mut self, frame: u64) {
        if frame <= self.start {
            return;
        }
        let drop = ((frame - self.start) as usize).min(self.len);
        if drop == 0 {
            return;
        }
        for p in &mut self.planes {
            p.copy_within(drop..self.len, 0);
        }
        self.len -= drop;
        self.start += drop as u64;
    }
}

/// Output side: synthesis overlap-adds here, `drain_into` hands finished frames
/// to the caller.
///
/// Three cursors, because an overlap-add tail lives past the finished region:
/// `read` is the next frame to hand out, `write` is how far synthesis has
/// declared frames final, and `filled` is how far any sample has been touched.
/// Everything at or past `filled` is guaranteed zero, which is what lets `add`
/// accumulate without clearing first.
pub struct OutputAccum {
    planes: Vec<Vec<f32>>,
    /// Running sum of the applied windows, when the engine overlap-adds.
    norm: Vec<f32>,
    capacity: usize,
    read: usize,
    write: usize,
    filled: usize,
    skip_pending: usize,
}

impl OutputAccum {
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self {
            planes: vec![vec![0.0; capacity]; channels],
            norm: Vec::new(),
            capacity,
            read: 0,
            write: 0,
            filled: 0,
            skip_pending: 0,
        }
    }

    /// Same, plus the window-sum plane an overlap-add engine needs.
    pub fn with_norm(channels: usize, capacity: usize) -> Self {
        let mut a = Self::new(channels, capacity);
        a.norm = vec![0.0; capacity];
        a
    }

    pub fn reset(&mut self) {
        self.read = 0;
        self.write = 0;
        self.filled = 0;
        self.skip_pending = 0;
        for p in &mut self.planes {
            p.fill(0.0);
        }
        self.norm.fill(0.0);
    }

    #[inline]
    pub fn pending(&self) -> usize {
        self.write - self.read
    }
    /// Frames of room past the write head for a new grain.
    #[inline]
    pub fn space(&self) -> usize {
        self.capacity - self.write
    }
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Slide everything down so `space()` grows back after a drain. Keeps the
    /// overlap-add tail past `write` intact.
    pub fn compact(&mut self) {
        if self.read == 0 {
            return;
        }
        let shift = self.read;
        let live = self.filled - shift;
        for p in &mut self.planes {
            p.copy_within(shift..self.filled, 0);
            p[live..self.filled].fill(0.0);
        }
        if !self.norm.is_empty() {
            self.norm.copy_within(shift..self.filled, 0);
            self.norm[live..self.filled].fill(0.0);
        }
        self.read = 0;
        self.write -= shift;
        self.filled = live;
    }

    /// Accumulate into the window-sum plane at `offset` past the write head.
    #[inline]
    pub fn add_norm(&mut self, offset: usize, w: f32) {
        let i = self.write + offset;
        if i < self.capacity {
            self.norm[i] += w;
            if i + 1 > self.filled {
                self.filled = i + 1;
            }
        }
    }

    /// Finalise `n` frames, dividing by the accumulated window sum.
    ///
    /// `eps` floors the divisor so an unpaired edge frame cannot be amplified
    /// into noise (dsp.md sec.4). Engines that use a flat-start / flat-tail
    /// window see a divisor of exactly 1 in the interior, so this is a safety
    /// net rather than a gain stage.
    pub fn advance_normalized(&mut self, n: usize, eps: f32) {
        let n = n.min(self.capacity - self.write);
        for i in self.write..self.write + n {
            let d = self.norm[i];
            let g = if d > eps { 1.0 / d } else { 0.0 };
            for p in &mut self.planes {
                p[i] *= g;
            }
        }
        self.advance(n);
    }

    /// Overlap-add at `offset` frames past the write head.
    #[inline]
    pub fn add(&mut self, c: usize, offset: usize, value: f32) {
        let i = self.write + offset;
        if i < self.capacity {
            self.planes[c][i] += value;
            if i + 1 > self.filled {
                self.filled = i + 1;
            }
        }
    }

    #[inline]
    pub fn get(&self, c: usize, offset: usize) -> f32 {
        let i = self.write + offset;
        if i < self.capacity {
            self.planes[c][i]
        } else {
            0.0
        }
    }

    /// Read a finished-or-pending frame at an absolute buffer offset from
    /// `read`. Used by WSOLA to correlate against what it already produced.
    #[inline]
    pub fn at_write_minus(&self, c: usize, back: usize) -> f32 {
        if back > self.write {
            0.0
        } else {
            self.planes[c][self.write - back]
        }
    }

    /// Declare `n` more frames final and drainable.
    pub fn advance(&mut self, n: usize) {
        self.write = (self.write + n).min(self.capacity);
        if self.write > self.filled {
            self.filled = self.write;
        }
    }

    /// Discard the next `n` finished frames instead of handing them out. Used
    /// for the overlap-add warm-up an engine renders before output frame 0.
    pub fn skip(&mut self, n: usize) {
        self.skip_pending += n;
    }

    /// Finished frames past the warm-up, available to read.
    ///
    /// `pending` counts everything finalised including the warm-up still to be
    /// discarded; a caller merging two accumulators has to compare what is
    /// actually deliverable, which is this.
    pub fn ready(&self) -> usize {
        self.pending().saturating_sub(self.skip_pending)
    }

    /// Read a ready frame without consuming it. `i` counts from the first
    /// deliverable frame.
    #[inline]
    pub fn ready_at(&self, c: usize, i: usize) -> f32 {
        let idx = self.read + self.skip_pending + i;
        if idx < self.write {
            self.planes[c][idx]
        } else {
            0.0
        }
    }

    /// Drop the warm-up if it is still there, then consume `n` ready frames.
    pub fn consume(&mut self, n: usize) {
        let s = self.skip_pending.min(self.pending());
        self.read += s;
        self.skip_pending -= s;
        self.read += n.min(self.pending());
    }

    /// Copy pending frames to `out` starting at `offset`. Returns the count.
    pub fn drain_into(&mut self, out: &mut AudioViewMut<'_>, offset: usize) -> usize {
        if self.skip_pending > 0 {
            let s = self.skip_pending.min(self.pending());
            self.read += s;
            self.skip_pending -= s;
        }
        let room = out.capacity().saturating_sub(offset);
        let n = self.pending().min(room);
        if n == 0 {
            return 0;
        }
        let (read, planes) = (self.read, &self.planes);
        for c in 0..out.channel_count().min(planes.len()) {
            let src = &planes[c][read..read + n];
            out.channel_mut(c)[offset..offset + n].copy_from_slice(src);
        }
        self.read += n;
        n
    }
}
