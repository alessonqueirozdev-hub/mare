//! Several devices as one.
//!
//! An [`Aggregate`] is a [`Device`] made of a master and any number of
//! followers. The master paces the hub: its periods are the aggregate's, and
//! its channels come first. Each follower runs on its own crystal, so it is
//! driven by a [`Follower`] on a thread of its own ([`Follower::spawn`]), and
//! its audio crosses to the master's clock through two `mare-drift` links:
//! what it records goes through one to the hub, resampled to the master's
//! clock, and what the hub mixes for it goes through the other, resampled to
//! its own (`docs/adr/0001`). Its channels follow the master's, followers in
//! the order given.
//!
//! Both sides stamp each period with the time it began, on one [`Clock`]
//! (the system's, or a test's), and nothing after [`aggregate`] allocates or
//! locks, on either side.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use mare_drift::{LinkConsumer, LinkProducer, LinkSettings, LinkStats, link};

use crate::clock::Clock;
use crate::device::{AudioBlock, Device, DeviceConfig, DeviceError, DeviceLatency};

/// A follower runs at another sample rate than the master. Drift is
/// corrected; a change of rate is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateMismatch {
    /// The follower's place in the list.
    pub index: usize,
    /// Its rate.
    pub rate: u32,
    /// The master's.
    pub master: u32,
}

impl fmt::Display for RateMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "follower {} runs at {} Hz, the master at {} Hz",
            self.index, self.rate, self.master
        )
    }
}

impl std::error::Error for RateMismatch {}

/// Joins `followers` to `master`, all timed by `clock`, and returns the
/// aggregate for the hub to drive and one [`Follower`] for each, to run.
pub fn aggregate<M: Device, F: Device>(
    master: M,
    followers: Vec<F>,
    clock: Arc<dyn Clock>,
) -> Result<(Aggregate<M>, Vec<Follower<F>>), RateMismatch> {
    let m = master.config();
    let mut config = m;
    let mut joined = Vec::with_capacity(followers.len());
    let mut sides = Vec::with_capacity(followers.len());
    for (index, device) in followers.into_iter().enumerate() {
        let f = device.config();
        if f.sample_rate != m.sample_rate {
            return Err(RateMismatch {
                index,
                rate: f.sample_rate,
                master: m.sample_rate,
            });
        }
        let (inputs, outputs) = (f.input_channels as usize, f.output_channels as usize);
        // What the follower records, in its packets, to the hub's cycles;
        // what the hub mixes, in its cycles, to the follower's periods.
        let (capture_in, capture_out) = match inputs {
            0 => (None, None),
            n => {
                let settings = LinkSettings::new(
                    n,
                    m.sample_rate,
                    f.max_frames as usize,
                    m.period_frames as usize,
                );
                let (p, c) = link(settings);
                (Some(p), Some(c))
            }
        };
        let (playback_in, playback_out) = match outputs {
            0 => (None, None),
            n => {
                let settings = LinkSettings::new(
                    n,
                    m.sample_rate,
                    m.max_frames as usize,
                    f.period_frames as usize,
                );
                let (p, c) = link(settings);
                (Some(p), Some(c))
            }
        };
        joined.push(Joined {
            first_input: config.input_channels as usize,
            inputs,
            first_output: config.output_channels as usize,
            outputs,
            capture: capture_out,
            playback: playback_in,
        });
        config.input_channels += f.input_channels;
        config.output_channels += f.output_channels;
        let stride = f.max_frames as usize;
        sides.push(Follower {
            clock: Arc::clone(&clock),
            inputs: vec![0.0; inputs * stride],
            outputs: vec![0.0; outputs * stride],
            interleaved: vec![0.0; inputs.max(outputs) * stride],
            stride,
            capture: capture_in,
            playback: playback_out,
            device,
        });
    }
    let widest = joined
        .iter()
        .map(|j| j.inputs.max(j.outputs))
        .max()
        .unwrap_or(0);
    let stride = m.max_frames as usize;
    let aggregate = Aggregate {
        master_outputs: vec![0.0; m.output_channels as usize * stride],
        interleaved: vec![0.0; widest * stride],
        stride,
        master,
        master_config: m,
        config,
        clock,
        joined,
        now_ns: 0,
    };
    Ok((aggregate, sides))
}

/// The hub's side of one follower.
struct Joined {
    first_input: usize,
    inputs: usize,
    first_output: usize,
    outputs: usize,
    capture: Option<LinkConsumer>,
    playback: Option<LinkProducer>,
}

/// A master device and the hub's side of its followers, driven as one
/// device by the hub.
pub struct Aggregate<M> {
    master: M,
    master_config: DeviceConfig,
    config: DeviceConfig,
    clock: Arc<dyn Clock>,
    joined: Vec<Joined>,
    /// The master's outputs, copied out of the hub's block to hand on.
    master_outputs: Vec<f32>,
    /// Frames to or from a link, interleaved.
    interleaved: Vec<f32>,
    stride: usize,
    /// When the current period began, in nanoseconds of the clock.
    now_ns: u64,
}

impl<M> Aggregate<M> {
    /// The master device.
    pub fn master(&self) -> &M {
        &self.master
    }

    /// The master device, to change.
    pub fn master_mut(&mut self) -> &mut M {
        &mut self.master
    }

    /// How many followers it has.
    pub fn followers(&self) -> usize {
        self.joined.len()
    }

    /// The counters of what follower `index` records, as the hub takes it;
    /// `None` for a follower that records nothing.
    pub fn capture_stats(&self, index: usize) -> Option<LinkStats> {
        self.joined[index].capture.as_ref().map(LinkConsumer::stats)
    }

    /// The latency, in frames, that resampling adds to what follower `index`
    /// records.
    pub fn capture_latency(&self, index: usize) -> Option<f64> {
        self.joined[index]
            .capture
            .as_ref()
            .map(LinkConsumer::latency)
    }

    /// The position, in follower `index`'s frames, of the next frame the hub
    /// takes from it. For tests that check where audio came from.
    pub fn capture_position(&self, index: usize) -> Option<f64> {
        self.joined[index]
            .capture
            .as_ref()
            .map(LinkConsumer::position)
    }
}

impl<M: Device> Device for Aggregate<M> {
    fn config(&self) -> DeviceConfig {
        self.config
    }

    /// The master's: the followers' channels have the latency their links
    /// add on top of their own devices' (see
    /// [`Aggregate::capture_latency`] and [`Follower::playback_latency`]).
    fn latency(&self) -> DeviceLatency {
        self.master.latency()
    }

    fn wait_period(&mut self) -> Result<usize, DeviceError> {
        let frames = self.master.wait_period()?;
        self.now_ns = self.clock.now().as_nanos() as u64;
        Ok(frames)
    }

    fn read(&mut self, inputs: &mut AudioBlock<'_>) -> Result<(), DeviceError> {
        let frames = inputs.frames();
        self.master
            .read(&mut inputs.channels_mut(0, self.master_config.input_channels as usize))?;
        for j in &mut self.joined {
            let Some(capture) = &mut j.capture else {
                continue;
            };
            let interleaved = &mut self.interleaved[..frames * j.inputs];
            capture.pull(interleaved, self.now_ns);
            for c in 0..j.inputs {
                let channel = inputs.channel_mut(j.first_input + c);
                for (i, sample) in channel.iter_mut().enumerate() {
                    *sample = interleaved[i * j.inputs + c];
                }
            }
        }
        Ok(())
    }

    fn write(&mut self, outputs: &AudioBlock<'_>) -> Result<(), DeviceError> {
        let frames = outputs.frames();
        let own = self.master_config.output_channels as usize;
        for c in 0..own {
            let start = c * self.stride;
            self.master_outputs[start..start + frames].copy_from_slice(outputs.channel(c));
        }
        self.master.write(&AudioBlock::new(
            &mut self.master_outputs,
            own,
            self.stride,
            frames,
        ))?;
        for j in &mut self.joined {
            let Some(playback) = &mut j.playback else {
                continue;
            };
            let interleaved = &mut self.interleaved[..frames * j.outputs];
            for c in 0..j.outputs {
                for (i, &sample) in outputs.channel(j.first_output + c).iter().enumerate() {
                    interleaved[i * j.outputs + c] = sample;
                }
            }
            playback.push(interleaved, self.now_ns);
        }
        Ok(())
    }
}

/// A follower device and its side of the links, run one period at a time.
pub struct Follower<F> {
    device: F,
    clock: Arc<dyn Clock>,
    capture: Option<LinkProducer>,
    playback: Option<LinkConsumer>,
    inputs: Vec<f32>,
    outputs: Vec<f32>,
    interleaved: Vec<f32>,
    stride: usize,
}

impl<F: Device> Follower<F> {
    /// Waits for the device's next period, hands what it recorded to the hub
    /// and gives it what the hub mixed for it.
    pub fn run_period(&mut self) -> Result<(), DeviceError> {
        let frames = self.device.wait_period()?.min(self.stride);
        let now_ns = self.clock.now().as_nanos() as u64;
        let (inputs, outputs) = (
            self.inputs.len() / self.stride,
            self.outputs.len() / self.stride,
        );
        let mut block = AudioBlock::new(&mut self.inputs, inputs, self.stride, frames);
        self.device.read(&mut block)?;
        if let Some(capture) = &mut self.capture {
            let interleaved = &mut self.interleaved[..frames * inputs];
            for c in 0..inputs {
                for (i, &sample) in block.channel(c).iter().enumerate() {
                    interleaved[i * inputs + c] = sample;
                }
            }
            capture.push(interleaved, now_ns);
        }
        let mut block = AudioBlock::new(&mut self.outputs, outputs, self.stride, frames);
        if let Some(playback) = &mut self.playback {
            let interleaved = &mut self.interleaved[..frames * outputs];
            playback.pull(interleaved, now_ns);
            for c in 0..outputs {
                for (i, sample) in block.channel_mut(c).iter_mut().enumerate() {
                    *sample = interleaved[i * outputs + c];
                }
            }
        }
        self.device.write(&block)
    }

    /// The device.
    pub fn device(&self) -> &F {
        &self.device
    }

    /// The device, to change.
    pub fn device_mut(&mut self) -> &mut F {
        &mut self.device
    }

    /// The counters of what the hub mixes for this follower, as it takes it;
    /// `None` for a follower that plays nothing.
    pub fn playback_stats(&self) -> Option<LinkStats> {
        self.playback.as_ref().map(LinkConsumer::stats)
    }

    /// The latency, in frames, that resampling adds to what this follower
    /// plays.
    pub fn playback_latency(&self) -> Option<f64> {
        self.playback.as_ref().map(LinkConsumer::latency)
    }

    /// The position, in the hub's frames, of the next frame this follower
    /// plays. For tests that check where audio came from.
    pub fn playback_position(&self) -> Option<f64> {
        self.playback.as_ref().map(LinkConsumer::position)
    }
}

impl<F: Device + 'static> Follower<F> {
    /// Runs the follower on a thread of its own, asked for real-time
    /// priority, until it is stopped or its device fails.
    pub fn spawn(self) -> std::io::Result<FollowerThread<F>> {
        let stop = Arc::new(AtomicBool::new(false));
        let realtime = Arc::new(AtomicBool::new(false));
        let handle = {
            let (stop, realtime) = (Arc::clone(&stop), Arc::clone(&realtime));
            std::thread::Builder::new()
                .name("mare-follower".into())
                .spawn(move || {
                    let mut follower = self;
                    // As for the hub's own thread, a refusal is reported,
                    // not fatal.
                    let promoted = mare_sys::promote_current_thread().ok();
                    let raised = promoted.as_ref().is_some_and(|p| p.is_promoted());
                    realtime.store(raised, Ordering::Release);
                    while !stop.load(Ordering::Acquire) {
                        if let Err(error) = follower.run_period() {
                            return (follower, Some(error));
                        }
                    }
                    (follower, None)
                })?
        };
        Ok(FollowerThread {
            stop,
            realtime,
            handle: Some(handle),
        })
    }
}

/// A follower running on its own thread.
pub struct FollowerThread<F> {
    stop: Arc<AtomicBool>,
    realtime: Arc<AtomicBool>,
    handle: Option<JoinHandle<(Follower<F>, Option<DeviceError>)>>,
}

impl<F> FollowerThread<F> {
    /// Whether the thread was granted real-time priority.
    pub fn is_realtime(&self) -> bool {
        self.realtime.load(Ordering::Acquire)
    }

    /// Whether the thread has stopped (its device failed).
    pub fn has_stopped(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops the thread after its current period and returns the follower,
    /// with the error that stopped it first, if one did.
    pub fn stop(mut self) -> (Follower<F>, Option<DeviceError>) {
        self.stop.store(true, Ordering::Release);
        let handle = self.handle.take().expect("joined only here or on drop");
        handle.join().expect("the follower thread panicked")
    }
}

impl<F> Drop for FollowerThread<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
