//! Frame units, channel layout, planar buffers and immutable source identity.
//!
//! One *frame* holds one sample for every channel. Source time and output time
//! are different clocks and get different newtypes so they cannot be swapped by
//! accident (system-design.md sec.5).

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! frame_unit {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            pub const ZERO: Self = Self(0);
            #[inline]
            pub fn get(self) -> u64 {
                self.0
            }
            #[inline]
            pub fn as_f64(self) -> f64 {
                self.0 as f64
            }
            #[inline]
            pub fn as_usize(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl std::ops::Add<u64> for $name {
            type Output = Self;
            #[inline]
            fn add(self, rhs: u64) -> Self {
                Self(self.0 + rhs)
            }
        }

        impl std::ops::Sub for $name {
            type Output = u64;
            #[inline]
            fn sub(self, rhs: Self) -> u64 {
                self.0 - rhs.0
            }
        }
    };
}

frame_unit!(SourceFrame, "Position in the immutable source file, in frames.");
frame_unit!(OutputFrame, "Position on the rendered output timeline, in frames.");

/// Channel layout supported by the first release.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    Mono,
    Stereo,
}

impl ChannelLayout {
    pub fn count(self) -> usize {
        match self {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
        }
    }
    pub fn from_count(n: usize) -> Result<Self, AudioError> {
        match n {
            1 => Ok(ChannelLayout::Mono),
            2 => Ok(ChannelLayout::Stereo),
            other => Err(AudioError::UnsupportedChannelCount(other)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioError {
    UnsupportedChannelCount(usize),
    RaggedChannels { expected: usize, found: usize },
    NoChannels,
    NonFinite { channel: usize, frame: usize },
    UnsupportedSampleRate(u32),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::UnsupportedChannelCount(n) => {
                write!(f, "unsupported channel count {n} (mono/stereo only)")
            }
            AudioError::RaggedChannels { expected, found } => {
                write!(f, "ragged planar buffer: expected {expected} frames, found {found}")
            }
            AudioError::NoChannels => write!(f, "buffer has no channels"),
            AudioError::NonFinite { channel, frame } => {
                write!(f, "non-finite sample at channel {channel}, frame {frame}")
            }
            AudioError::UnsupportedSampleRate(r) => write!(f, "unsupported sample rate {r}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Owned planar `f32` buffer. Used by the offline path and by test fixtures;
/// the real-time path borrows slices through [`AudioView`] instead.
#[derive(Clone, PartialEq)]
pub struct AudioBuffer {
    channels: Vec<Vec<f32>>,
    frames: usize,
}

impl fmt::Debug for AudioBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioBuffer")
            .field("channels", &self.channels.len())
            .field("frames", &self.frames)
            .finish()
    }
}

impl AudioBuffer {
    pub fn silence(channels: usize, frames: usize) -> Self {
        Self { channels: vec![vec![0.0; frames]; channels.max(1)], frames }
    }

    pub fn from_planar(channels: Vec<Vec<f32>>) -> Result<Self, AudioError> {
        if channels.is_empty() {
            return Err(AudioError::NoChannels);
        }
        let frames = channels[0].len();
        for ch in &channels {
            if ch.len() != frames {
                return Err(AudioError::RaggedChannels { expected: frames, found: ch.len() });
            }
        }
        Ok(Self { channels, frames })
    }

    pub fn from_interleaved(data: &[f32], channels: usize) -> Result<Self, AudioError> {
        if channels == 0 {
            return Err(AudioError::NoChannels);
        }
        let frames = data.len() / channels;
        let mut planar = vec![vec![0.0f32; frames]; channels];
        for f in 0..frames {
            for (c, plane) in planar.iter_mut().enumerate() {
                plane[f] = data[f * channels + c];
            }
        }
        Ok(Self { channels: planar, frames })
    }

    pub fn to_interleaved(&self) -> Vec<f32> {
        let nch = self.channels.len();
        let mut out = vec![0.0f32; self.frames * nch];
        for f in 0..self.frames {
            for (c, plane) in self.channels.iter().enumerate() {
                out[f * nch + c] = plane[f];
            }
        }
        out
    }

    #[inline]
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }
    #[inline]
    pub fn frames(&self) -> usize {
        self.frames
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }
    #[inline]
    pub fn channel(&self, c: usize) -> &[f32] {
        &self.channels[c]
    }
    #[inline]
    pub fn channel_mut(&mut self, c: usize) -> &mut [f32] {
        &mut self.channels[c]
    }
    pub fn planes(&self) -> &[Vec<f32>] {
        &self.channels
    }

    /// Reject NaN/Inf before prepare, per the process contract (item 7).
    pub fn check_finite(&self) -> Result<(), AudioError> {
        for (c, plane) in self.channels.iter().enumerate() {
            for (f, s) in plane.iter().enumerate() {
                if !s.is_finite() {
                    return Err(AudioError::NonFinite { channel: c, frame: f });
                }
            }
        }
        Ok(())
    }

    pub fn peak(&self) -> f32 {
        self.channels.iter().flat_map(|c| c.iter()).fold(0.0f32, |a, s| a.max(s.abs()))
    }

    pub fn slice(&self, start: usize, len: usize) -> AudioBuffer {
        let end = (start + len).min(self.frames);
        let start = start.min(end);
        AudioBuffer::from_planar(self.channels.iter().map(|c| c[start..end].to_vec()).collect())
            .expect("slice keeps layout")
    }

    /// Mono sum, for analysis and display only.
    pub fn mono_sum(&self) -> Vec<f32> {
        let n = self.channels.len() as f32;
        let mut out = vec![0.0f32; self.frames];
        for plane in &self.channels {
            for (o, s) in out.iter_mut().zip(plane.iter()) {
                *o += *s;
            }
        }
        for o in out.iter_mut() {
            *o /= n;
        }
        out
    }
}

/// Borrowed, already validated planar input for `process()`.
///
/// A host hands over borrowed channel slices; the offline path and the engine
/// chain already own `Vec` planes. Both shapes are accepted without copying and
/// without allocating a slice-of-slices per call.
#[derive(Copy, Clone)]
pub struct AudioView<'a> {
    chans: PlanesRef<'a>,
    offset: usize,
    frames: usize,
}

#[derive(Copy, Clone)]
enum PlanesRef<'a> {
    Slices(&'a [&'a [f32]]),
    Planar { planes: &'a [Vec<f32>] },
}

impl<'a> AudioView<'a> {
    pub fn new(chans: &'a [&'a [f32]]) -> Result<Self, AudioError> {
        if chans.is_empty() {
            return Err(AudioError::NoChannels);
        }
        let frames = chans[0].len();
        for c in chans {
            if c.len() != frames {
                return Err(AudioError::RaggedChannels { expected: frames, found: c.len() });
            }
        }
        Ok(Self { chans: PlanesRef::Slices(chans), offset: 0, frames })
    }

    /// A window `[offset, offset+frames)` over owned planes.
    pub fn from_planar(
        planes: &'a [Vec<f32>],
        offset: usize,
        frames: usize,
    ) -> Result<Self, AudioError> {
        if planes.is_empty() {
            return Err(AudioError::NoChannels);
        }
        for p in planes {
            if p.len() < offset + frames {
                return Err(AudioError::RaggedChannels {
                    expected: offset + frames,
                    found: p.len(),
                });
            }
        }
        Ok(Self { chans: PlanesRef::Planar { planes }, offset, frames })
    }

    #[inline]
    pub fn channel_count(&self) -> usize {
        match self.chans {
            PlanesRef::Slices(s) => s.len(),
            PlanesRef::Planar { planes } => planes.len(),
        }
    }
    #[inline]
    pub fn frames(&self) -> usize {
        self.frames
    }
    #[inline]
    pub fn channel(&self, c: usize) -> &[f32] {
        let (o, n) = (self.offset, self.frames);
        match self.chans {
            PlanesRef::Slices(s) => &s[c][o..o + n],
            PlanesRef::Planar { planes } => &planes[c][o..o + n],
        }
    }

    /// The same buffer from `from` frames in. Used to re-offer input a
    /// downstream stage has not consumed yet, without copying.
    pub fn sub(&self, from: usize) -> Self {
        let from = from.min(self.frames);
        Self { chans: self.chans, offset: self.offset + from, frames: self.frames - from }
    }
}

/// Borrowed, already validated planar output for `process()`.
pub struct AudioViewMut<'a> {
    chans: PlanesMut<'a>,
    frames: usize,
}

enum PlanesMut<'a> {
    Slices(&'a mut [&'a mut [f32]]),
    Planar { planes: &'a mut [Vec<f32>], offset: usize },
}

impl<'a> AudioViewMut<'a> {
    pub fn new(chans: &'a mut [&'a mut [f32]]) -> Result<Self, AudioError> {
        if chans.is_empty() {
            return Err(AudioError::NoChannels);
        }
        let frames = chans[0].len();
        for c in chans.iter() {
            if c.len() != frames {
                return Err(AudioError::RaggedChannels { expected: frames, found: c.len() });
            }
        }
        Ok(Self { chans: PlanesMut::Slices(chans), frames })
    }

    /// A writable window `[offset, offset+frames)` over owned planes.
    pub fn from_planar(
        planes: &'a mut [Vec<f32>],
        offset: usize,
        frames: usize,
    ) -> Result<Self, AudioError> {
        if planes.is_empty() {
            return Err(AudioError::NoChannels);
        }
        for p in planes.iter() {
            if p.len() < offset + frames {
                return Err(AudioError::RaggedChannels {
                    expected: offset + frames,
                    found: p.len(),
                });
            }
        }
        Ok(Self { chans: PlanesMut::Planar { planes, offset }, frames })
    }

    #[inline]
    pub fn channel_count(&self) -> usize {
        match &self.chans {
            PlanesMut::Slices(s) => s.len(),
            PlanesMut::Planar { planes, .. } => planes.len(),
        }
    }
    #[inline]
    pub fn capacity(&self) -> usize {
        self.frames
    }
    #[inline]
    pub fn channel_mut(&mut self, c: usize) -> &mut [f32] {
        let n = self.frames;
        match &mut self.chans {
            PlanesMut::Slices(s) => &mut s[c][..n],
            PlanesMut::Planar { planes, offset } => {
                let o = *offset;
                &mut planes[c][o..o + n]
            }
        }
    }
}

/// Content identity of a source. `id` is a 128-bit FNV-1a digest over the
/// sample data and format: not cryptographic, but stable across paths and
/// mtimes, which is what the cache keys need (system-design.md sec.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub id: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub frames: u64,
}

impl SourceIdentity {
    pub fn of(buf: &AudioBuffer, sample_rate: u32) -> Self {
        let mut h = Fnv1a128::new();
        h.write_u64(sample_rate as u64);
        h.write_u64(buf.channel_count() as u64);
        h.write_u64(buf.frames() as u64);
        for plane in buf.planes() {
            for s in plane {
                h.write_u64(s.to_bits() as u64);
            }
        }
        Self {
            id: h.finish_hex(),
            sample_rate,
            channels: buf.channel_count(),
            frames: buf.frames() as u64,
        }
    }
    pub fn source_frames(&self) -> SourceFrame {
        SourceFrame(self.frames)
    }
}

/// 128-bit FNV-1a. Used for content and cache keys, never for security.
pub struct Fnv1a128 {
    lo: u64,
    hi: u64,
}

impl Default for Fnv1a128 {
    fn default() -> Self {
        Self::new()
    }
}

impl Fnv1a128 {
    const PRIME_LO: u64 = 0x0000_0000_0000_013b;
    pub fn new() -> Self {
        Self { lo: 0x62b8_2175_6295_c58d, hi: 0x6c62_272e_07bb_0142 }
    }
    pub fn write_u64(&mut self, v: u64) {
        for i in 0..8 {
            self.write_u8(((v >> (i * 8)) & 0xff) as u8);
        }
    }
    pub fn write_str(&mut self, s: &str) {
        for b in s.as_bytes() {
            self.write_u8(*b);
        }
    }
    pub fn write_f64(&mut self, v: f64) {
        self.write_u64(v.to_bits());
    }
    /// 128-bit multiply by the FNV prime 2^88 + 0x13b, done in 64-bit halves.
    pub fn write_u8(&mut self, b: u8) {
        self.lo ^= b as u64;
        let prod = (self.lo as u128) * (Self::PRIME_LO as u128);
        let nlo = prod as u64;
        let carry = (prod >> 64) as u64;
        let nhi = self
            .hi
            .wrapping_mul(Self::PRIME_LO)
            .wrapping_add(carry)
            .wrapping_add(self.lo << 24);
        self.lo = nlo;
        self.hi = nhi;
    }
    pub fn finish_hex(&self) -> String {
        format!("{:016x}{:016x}", self.hi, self.lo)
    }
}

/// Sample rates covered by the test matrix. Anything else must be declared as a
/// capability before it is accepted (system-design.md sec.1).
pub const TESTED_RATES: [u32; 3] = [44_100, 48_000, 96_000];

pub fn check_rate(rate: u32) -> Result<(), AudioError> {
    if rate == 0 || rate > 768_000 {
        return Err(AudioError::UnsupportedSampleRate(rate));
    }
    Ok(())
}
