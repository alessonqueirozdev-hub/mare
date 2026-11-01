//! What the hub needs from an audio device.

use std::fmt;

/// How a device runs, fixed while it is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceConfig {
    /// Frames per second.
    pub sample_rate: u32,
    /// Frames in a period, as the device usually asks for them.
    pub period_frames: u32,
    /// The most frames the device may ask for in one period.
    pub max_frames: u32,
    /// Channels the device records.
    pub input_channels: u32,
    /// Channels the device plays.
    pub output_channels: u32,
}

/// A device stopped working.
#[derive(Debug)]
#[non_exhaustive]
pub enum DeviceError {
    /// The device is gone (unplugged, or taken by another program).
    Lost,
    /// The device stopped asking for periods.
    Timeout,
    /// The operating system reported an error, with this code.
    Os(i64),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost => write!(f, "the device is gone"),
            Self::Timeout => write!(f, "the device stopped asking for periods"),
            Self::Os(code) => write!(f, "the device failed with code {code:#x}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// Audio for one period, one buffer per channel, not interleaved.
pub struct AudioBlock<'a> {
    data: &'a mut [f32],
    channels: usize,
    stride: usize,
    frames: usize,
}

impl<'a> AudioBlock<'a> {
    /// A block over `data`, which holds `channels` buffers of `stride`
    /// samples, of which the first `frames` are this period's.
    ///
    /// # Panics
    ///
    /// If `data` is too short or `frames > stride`.
    pub fn new(data: &'a mut [f32], channels: usize, stride: usize, frames: usize) -> Self {
        assert!(frames <= stride && data.len() >= channels * stride);
        Self {
            data,
            channels,
            stride,
            frames,
        }
    }

    /// Channels in the block.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames in the period.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Channel `index`'s samples.
    pub fn channel(&self, index: usize) -> &[f32] {
        let start = index * self.stride;
        &self.data[start..start + self.frames]
    }

    /// Channel `index`'s samples, to write.
    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        let start = index * self.stride;
        &mut self.data[start..start + self.frames]
    }

    /// The `count` channels from `first` on, as a block of their own.
    ///
    /// # Panics
    ///
    /// If they go past the block's last channel.
    pub fn channels_mut(&mut self, first: usize, count: usize) -> AudioBlock<'_> {
        assert!(first + count <= self.channels);
        let start = first * self.stride;
        AudioBlock {
            data: &mut self.data[start..start + count * self.stride],
            channels: count,
            stride: self.stride,
            frames: self.frames,
        }
    }
}

/// An audio device the hub drives.
///
/// The hub calls these from its cycle thread, in this order, once a period:
/// [`wait_period`](Device::wait_period), [`read`](Device::read),
/// [`write`](Device::write). They must not allocate or lock; waiting is
/// allowed only in `wait_period`.
pub trait Device: Send {
    /// How the device runs.
    fn config(&self) -> DeviceConfig;

    /// Waits until the device wants the next period, and returns its length
    /// in frames (at most `max_frames`).
    fn wait_period(&mut self) -> Result<usize, DeviceError>;

    /// Fills `inputs` with what the device recorded this period.
    fn read(&mut self, inputs: &mut AudioBlock<'_>) -> Result<(), DeviceError>;

    /// Hands the device what to play this period.
    fn write(&mut self, outputs: &AudioBlock<'_>) -> Result<(), DeviceError>;
}

impl<D: Device + ?Sized> Device for Box<D> {
    fn config(&self) -> DeviceConfig {
        (**self).config()
    }

    fn wait_period(&mut self) -> Result<usize, DeviceError> {
        (**self).wait_period()
    }

    fn read(&mut self, inputs: &mut AudioBlock<'_>) -> Result<(), DeviceError> {
        (**self).read(inputs)
    }

    fn write(&mut self, outputs: &AudioBlock<'_>) -> Result<(), DeviceError> {
        (**self).write(outputs)
    }
}
