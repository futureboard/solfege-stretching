//! Minimal RIFF/WAVE reader and writer.
//!
//! File IO lives here and in the CLI, never in the DSP library proper
//! (system-design.md sec.3). Reads PCM 8/16/24/32 and IEEE float 32/64;
//! writes 16-bit PCM or 32-bit float. Quantisation is an export-layer concern,
//! so the internal path stays float and this module is the only place a sample
//! changes representation.

use crate::audio::AudioBuffer;
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub enum WavError {
    Io(String),
    NotRiff,
    NotWave,
    MissingFmt,
    MissingData,
    UnsupportedFormat { format: u16, bits: u16 },
    UnsupportedChannels(usize),
    Truncated,
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::Io(e) => write!(f, "io error: {e}"),
            WavError::NotRiff => write!(f, "not a RIFF file"),
            WavError::NotWave => write!(f, "not a WAVE file"),
            WavError::MissingFmt => write!(f, "no fmt chunk"),
            WavError::MissingData => write!(f, "no data chunk"),
            WavError::UnsupportedFormat { format, bits } => {
                write!(f, "unsupported wav format tag {format} at {bits} bits")
            }
            WavError::UnsupportedChannels(n) => {
                write!(f, "{n} channels; this build handles mono and stereo")
            }
            WavError::Truncated => write!(f, "data chunk is truncated"),
        }
    }
}

impl std::error::Error for WavError {}

impl From<std::io::Error> for WavError {
    fn from(e: std::io::Error) -> Self {
        WavError::Io(e.to_string())
    }
}

pub struct WavFile {
    pub audio: AudioBuffer,
    pub sample_rate: u32,
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

pub fn read<P: AsRef<Path>>(path: P) -> Result<WavFile, WavError> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    parse(&bytes)
}

pub fn parse(bytes: &[u8]) -> Result<WavFile, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut data: Option<(usize, usize)> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(&bytes[pos + 4..pos + 8]) as usize;
        let body = pos + 8;
        if body + size > bytes.len() && id != b"data" {
            break;
        }
        if id == b"fmt " && size >= 16 {
            let format = u16le(&bytes[body..]);
            let channels = u16le(&bytes[body + 2..]);
            let rate = u32le(&bytes[body + 4..]);
            let bits = u16le(&bytes[body + 14..]);
            // WAVE_FORMAT_EXTENSIBLE carries the real tag in the GUID prefix.
            let format = if format == 0xFFFE && size >= 26 {
                u16le(&bytes[body + 24..])
            } else {
                format
            };
            fmt = Some((format, channels, rate, bits));
        } else if id == b"data" {
            let end = (body + size).min(bytes.len());
            data = Some((body, end));
        }
        pos = body + size + (size & 1);
    }

    let (format, channels, rate, bits) = fmt.ok_or(WavError::MissingFmt)?;
    let (start, end) = data.ok_or(WavError::MissingData)?;
    let channels = channels as usize;
    if channels == 0 || channels > 2 {
        return Err(WavError::UnsupportedChannels(channels));
    }
    let raw = &bytes[start..end];
    let bytes_per_sample = (bits as usize) / 8;
    if bytes_per_sample == 0 {
        return Err(WavError::UnsupportedFormat { format, bits });
    }
    let frame_bytes = bytes_per_sample * channels;
    if frame_bytes == 0 {
        return Err(WavError::Truncated);
    }
    let frames = raw.len() / frame_bytes;

    let mut planes = vec![vec![0.0f32; frames]; channels];
    for f in 0..frames {
        for (c, plane) in planes.iter_mut().enumerate() {
            let o = f * frame_bytes + c * bytes_per_sample;
            let s = &raw[o..o + bytes_per_sample];
            plane[f] = match (format, bits) {
                (1, 8) => (s[0] as f32 - 128.0) / 128.0,
                (1, 16) => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                (1, 24) => {
                    let v = ((s[2] as i32) << 24 | (s[1] as i32) << 16 | (s[0] as i32) << 8) >> 8;
                    v as f32 / 8_388_608.0
                }
                (1, 32) => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
                (3, 32) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                (3, 64) => f64::from_le_bytes([
                    s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
                ]) as f32,
                _ => return Err(WavError::UnsupportedFormat { format, bits }),
            };
        }
    }

    let audio = AudioBuffer::from_planar(planes).map_err(|_| WavError::Truncated)?;
    Ok(WavFile { audio, sample_rate: rate })
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WriteFormat {
    /// 16-bit PCM with rounding. No dither is applied and none is claimed.
    Pcm16,
    Float32,
}

pub fn write<P: AsRef<Path>>(
    path: P,
    audio: &AudioBuffer,
    sample_rate: u32,
    format: WriteFormat,
) -> Result<(), WavError> {
    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    let channels = audio.channel_count() as u16;
    let frames = audio.frames();
    let (tag, bits) = match format {
        WriteFormat::Pcm16 => (1u16, 16u16),
        WriteFormat::Float32 => (3u16, 32u16),
    };
    let bytes_per_sample = (bits / 8) as u32;
    let block_align = channels as u32 * bytes_per_sample;
    let data_len = frames as u32 * block_align;

    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&tag.to_le_bytes())?;
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * block_align).to_le_bytes())?;
    w.write_all(&(block_align as u16).to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;

    for f in 0..frames {
        for c in 0..audio.channel_count() {
            let v = audio.channel(c)[f];
            match format {
                WriteFormat::Pcm16 => {
                    let q = (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    w.write_all(&q.to_le_bytes())?;
                }
                WriteFormat::Float32 => w.write_all(&v.to_le_bytes())?,
            }
        }
    }
    w.flush()?;
    Ok(())
}
