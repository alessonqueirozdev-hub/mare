//! A device that exists only in memory, for tests.
//!
//! A [`FakeDevice`] records whatever its input function says and keeps the
//! last period it was given to play, so a test can check what the hub mixed.
//! It keeps time in one of two ways: on a [`FakeClock`], which it moves one
//! period forward each time the hub asks for a period (no waiting at all), or
//! in real time, sleeping until each period is due. Its crystal can be made
//! fast or slow by a fraction ([`FakeDevice::with_skew`]), as a second
//! device's is against the first's.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::FakeClock;
use crate::device::{AudioBlock, Device, DeviceConfig, DeviceError};

/// How a fake device keeps time.
#[derive(Clone, Debug)]
pub enum Pacing {
    /// Each period starts the moment the hub asks for it, and moves this
    /// clock to the period's start.
    Fake(Arc<FakeClock>),
    /// Each period starts when real time reaches it.
    RealTime,
}

/// What a fake device records: the sample of `channel` at frame `frame`.
pub type InputFn = fn(channel: u32, frame: u64) -> f32;

/// Silence.
pub fn silence(_channel: u32, _frame: u64) -> f32 {
    0.0
}

/// A device in memory.
pub struct FakeDevice {
    config: DeviceConfig,
    pacing: Pacing,
    origin: Option<Instant>,
    input: InputFn,
    /// How fast its clock runs against true time, as a fraction.
    skew: f64,
    /// Frames handed out so far; the next period starts here.
    frame: u64,
    /// The current period's first frame and length.
    period_start: u64,
    period_frames: usize,
    periods: u64,
    last_output: Vec<f32>,
}

impl FakeDevice {
    /// A device with `config`, keeping time by `pacing`, that records
    /// silence.
    pub fn new(config: DeviceConfig, pacing: Pacing) -> Self {
        Self {
            config,
            pacing,
            origin: None,
            input: silence,
            skew: 0.0,
            frame: 0,
            period_start: 0,
            period_frames: 0,
            periods: 0,
            last_output: vec![0.0; (config.output_channels * config.max_frames) as usize],
        }
    }

    /// The same device, recording what `input` says instead.
    pub fn with_input(mut self, input: InputFn) -> Self {
        self.input = input;
        self
    }

    /// The same device, its clock `skew` fast (or slow, if negative): at
    /// 100e-6, its 48 000 frames take 1 s / 1.0001.
    pub fn with_skew(mut self, skew: f64) -> Self {
        self.skew = skew;
        self
    }

    /// Periods played so far.
    pub fn periods(&self) -> u64 {
        self.periods
    }

    /// The first frame of the last period played.
    pub fn last_period_start(&self) -> u64 {
        self.period_start
    }

    /// What the device played on `channel` in the last period.
    pub fn last_output(&self, channel: u32) -> &[f32] {
        let stride = self.config.max_frames as usize;
        let start = channel as usize * stride;
        &self.last_output[start..start + self.period_frames]
    }

    fn time_of(&self, frame: u64) -> Duration {
        if self.skew != 0.0 {
            let rate = f64::from(self.config.sample_rate) * (1.0 + self.skew);
            return Duration::from_secs_f64(frame as f64 / rate);
        }
        let nanos = u128::from(frame) * 1_000_000_000 / u128::from(self.config.sample_rate);
        Duration::from_nanos(nanos as u64)
    }
}

impl Device for FakeDevice {
    fn config(&self) -> DeviceConfig {
        self.config
    }

    fn wait_period(&mut self) -> Result<usize, DeviceError> {
        self.period_start = self.frame;
        self.period_frames = self.config.period_frames as usize;
        self.frame += u64::from(self.config.period_frames);
        let start = self.time_of(self.period_start);
        match &self.pacing {
            Pacing::Fake(clock) => clock.advance_to(start),
            Pacing::RealTime => {
                let origin = *self.origin.get_or_insert_with(Instant::now);
                let due = origin + start;
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
            }
        }
        Ok(self.period_frames)
    }

    fn read(&mut self, inputs: &mut AudioBlock<'_>) -> Result<(), DeviceError> {
        for channel in 0..inputs.channels() {
            let start = self.period_start;
            for (i, sample) in inputs.channel_mut(channel).iter_mut().enumerate() {
                *sample = (self.input)(channel as u32, start + i as u64);
            }
        }
        Ok(())
    }

    fn write(&mut self, outputs: &AudioBlock<'_>) -> Result<(), DeviceError> {
        let stride = self.config.max_frames as usize;
        for channel in 0..outputs.channels() {
            let start = channel * stride;
            self.last_output[start..start + outputs.frames()]
                .copy_from_slice(outputs.channel(channel));
        }
        self.periods += 1;
        Ok(())
    }
}
