//! Sample formats a device's buffer may hold, and conversion to and from the
//! hub's 32-bit float.
//!
//! In exclusive mode the device's own format is used, which is often integer.
//! Floats are clamped to −1..=1 and scaled by 2^(bits−1) − 1, rounding to the
//! nearest integer, so that +1 and −1 map to the same magnitude and a round
//! trip through any integer format changes a sample by at most half a step.
//! Nothing here allocates.

use mare_hub::AudioBlock;

/// How each sample of an interleaved buffer is stored, little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit IEEE float.
    F32,
    /// 16-bit signed integer.
    I16,
    /// 24-bit signed integer in three bytes.
    I24,
    /// 24-bit signed integer in the top three bytes of four.
    I24In32,
    /// 32-bit signed integer.
    I32,
}

impl SampleFormat {
    /// Bytes per sample.
    pub const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::F32 | Self::I24In32 | Self::I32 => 4,
        }
    }

    /// Bits that carry the sample.
    pub const fn valid_bits(self) -> u16 {
        match self {
            Self::I16 => 16,
            Self::I24 | Self::I24In32 => 24,
            Self::F32 | Self::I32 => 32,
        }
    }
}

fn to_int(x: f32, bits: u32) -> i32 {
    let scale = ((1u64 << (bits - 1)) - 1) as f64;
    // NaN becomes silence rather than full scale.
    let x = if x.is_nan() {
        0.0
    } else {
        f64::from(x).clamp(-1.0, 1.0)
    };
    (x * scale).round() as i32
}

fn from_int(v: i32, bits: u32) -> f32 {
    let scale = ((1u64 << (bits - 1)) - 1) as f64;
    (f64::from(v) / scale).clamp(-1.0, 1.0) as f32
}

fn store(format: SampleFormat, x: f32, out: &mut [u8]) {
    match format {
        SampleFormat::F32 => out.copy_from_slice(&x.to_le_bytes()),
        SampleFormat::I16 => out.copy_from_slice(&(to_int(x, 16) as i16).to_le_bytes()),
        SampleFormat::I24 => out.copy_from_slice(&to_int(x, 24).to_le_bytes()[..3]),
        SampleFormat::I24In32 => out.copy_from_slice(&(to_int(x, 24) << 8).to_le_bytes()),
        SampleFormat::I32 => out.copy_from_slice(&to_int(x, 32).to_le_bytes()),
    }
}

pub(crate) fn load(format: SampleFormat, bytes: &[u8]) -> f32 {
    match format {
        SampleFormat::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        SampleFormat::I16 => from_int(i32::from(i16::from_le_bytes([bytes[0], bytes[1]])), 16),
        SampleFormat::I24 => {
            // Sign-extend through the top byte.
            from_int(
                i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8,
                24,
            )
        }
        SampleFormat::I24In32 => from_int(
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 8,
            24,
        ),
        SampleFormat::I32 => from_int(
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            32,
        ),
    }
}

/// Interleaves `block`'s channels into `out` in `format`, `channels` samples
/// per frame. Device channels past the block's are written as silence.
///
/// # Panics
///
/// If `out` is shorter than the block's frames need.
pub fn interleave(block: &AudioBlock<'_>, format: SampleFormat, channels: usize, out: &mut [u8]) {
    let size = format.bytes();
    let frames = block.frames();
    assert!(out.len() >= frames * channels * size);
    for channel in 0..channels {
        if channel < block.channels() {
            let samples = block.channel(channel);
            for (frame, &x) in samples.iter().enumerate() {
                let at = (frame * channels + channel) * size;
                store(format, x, &mut out[at..at + size]);
            }
        } else {
            for frame in 0..frames {
                let at = (frame * channels + channel) * size;
                store(format, 0.0, &mut out[at..at + size]);
            }
        }
    }
}

/// Splits interleaved `input` in `format`, `channels` samples per frame, into
/// `block`'s channels. Block channels past the device's are filled with
/// silence.
///
/// # Panics
///
/// If `input` is shorter than the block's frames need.
pub fn deinterleave(
    input: &[u8],
    format: SampleFormat,
    channels: usize,
    block: &mut AudioBlock<'_>,
) {
    let size = format.bytes();
    let frames = block.frames();
    assert!(input.len() >= frames * channels * size);
    for channel in 0..block.channels() {
        let samples = block.channel_mut(channel);
        if channel < channels {
            for (frame, x) in samples.iter_mut().enumerate() {
                let at = (frame * channels + channel) * size;
                *x = load(format, &input[at..at + size]);
            }
        } else {
            samples.fill(0.0);
        }
    }
}
