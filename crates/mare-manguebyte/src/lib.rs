//! MARÉ as an audio backend of the Manguebyte Audio Framework.
//!
//! [`MareBackend`] implements `manguebyte_devices::AudioBackend`: handed to
//! `DeviceManager::with_audio_backend`, it makes every MARÉ hub the program
//! knows of a device, both input and output, and runs the program's audio
//! as a client of the hub it chooses. A program built on Manguebyte (a
//! standalone plugin, through `Standalone::start_on`) then plays through
//! the hub without changing its own code: the hub owns the sound card, in
//! exclusive mode at a few milliseconds, and the program shares it with
//! every other client.
//!
//! ```no_run
//! use manguebyte_devices::DeviceManager;
//! use mare_manguebyte::MareBackend;
//!
//! let manager = DeviceManager::with_audio_backend(MareBackend::new("caecilia"));
//! # let _ = manager;
//! ```
//!
//! A hub runs at its own sample rate and period, which the program gets as
//! the device's only configuration; it may ask for as many channels as it
//! likes, and those past the hub's device are silent. The program's
//! callback runs on the client's cycle thread, raised to real-time
//! priority, and nothing on that path allocates or locks. MIDI stays with
//! the device manager, which opens the system's ports itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use manguebyte_core::AudioBuffer;
use manguebyte_devices::{
    AudioBackend, BackendCallback, BackendEndpoint, BackendFormat, BackendRequest, BackendStream,
    CallbackInfo, ConfigRange, DeviceError, DeviceInfo, DeviceSnapshot, DeviceSummary, Direction,
    StreamSettings, StreamStatus,
};
use mare_client::{Client, ClientConfig, ConnectError, HubInfo};
use mare_sys::HubName;

/// The host name a device setup gives this backend.
pub const HOST_NAME: &str = "MARÉ";

/// What a device's identifier starts with; the hub's name follows.
const ID_PREFIX: &str = "mare:";

/// The channels a hub that does not say its device's is taken to have.
const UNKNOWN_CHANNELS: u32 = 2;

/// The identifier of the device that is the hub called `hub`.
pub fn device_id(hub: &HubName) -> String {
    format!("{ID_PREFIX}{}", hub.as_str())
}

/// The hub a device identifier names.
fn hub_of(id: &str) -> Result<HubName, DeviceError> {
    id.strip_prefix(ID_PREFIX)
        .and_then(|name| HubName::new(name).ok())
        .ok_or_else(|| DeviceError::DeviceNotFound(id.to_owned()))
}

fn device_error(hub: &HubName, error: ConnectError) -> DeviceError {
    match error {
        ConnectError::NoHub(_) | ConnectError::HubNotRunning => {
            DeviceError::DeviceNotFound(device_id(hub))
        }
        other => DeviceError::Other(Box::new(other)),
    }
}

fn channels(info: &HubInfo, direction: Direction) -> u32 {
    let reported = match direction {
        Direction::Input => info.device_inputs,
        Direction::Output => info.device_outputs,
    };
    if reported == 0 {
        UNKNOWN_CHANNELS
    } else {
        reported
    }
}

/// MARÉ's hubs, as a Manguebyte device manager's devices.
#[derive(Debug, Clone)]
pub struct MareBackend {
    client_name: String,
    hubs: Vec<HubName>,
}

impl MareBackend {
    /// A backend whose clients are called `client_name` (as the hub lists
    /// them), looking for the default hub of the user's session.
    pub fn new(client_name: &str) -> Self {
        Self {
            client_name: client_name.to_owned(),
            hubs: vec![HubName::default_hub()],
        }
    }

    /// The same backend, looking for these hubs instead, the first being
    /// the default device.
    #[must_use]
    pub fn with_hubs(mut self, hubs: impl IntoIterator<Item = HubName>) -> Self {
        self.hubs = hubs.into_iter().collect();
        self
    }

    fn summary(hub: &HubName) -> DeviceSummary {
        DeviceSummary {
            id: device_id(hub),
            name: format!("{HOST_NAME}: {}", hub.as_str()),
        }
    }
}

impl AudioBackend for MareBackend {
    fn host_name(&self) -> String {
        HOST_NAME.to_owned()
    }

    fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError> {
        let mut snapshot = DeviceSnapshot::default();
        for hub in &self.hubs {
            let Ok(info) = mare_client::probe(hub) else {
                continue;
            };
            let summary = Self::summary(hub);
            if info.device_inputs > 0 || info.device_outputs == 0 {
                snapshot.inputs.push(summary.clone());
                snapshot
                    .default_input
                    .get_or_insert_with(|| summary.id.clone());
            }
            if info.device_outputs > 0 || info.device_inputs == 0 {
                snapshot
                    .default_output
                    .get_or_insert_with(|| summary.id.clone());
                snapshot.outputs.push(summary);
            }
        }
        Ok(snapshot)
    }

    fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
        let hub = hub_of(id)?;
        let info = mare_client::probe(&hub).map_err(|e| device_error(&hub, e))?;
        let most = channels(&info, direction);
        Ok(DeviceInfo {
            id: device_id(&hub),
            name: Self::summary(&hub).name,
            is_default: self.hubs.first() == Some(&hub),
            default_sample_rate: Some(info.sample_rate),
            default_channels: Some(most.min(u32::from(u16::MAX)) as u16),
            configs: (1..=most.min(u32::from(u16::MAX)) as u16)
                .map(|channels| ConfigRange {
                    channels,
                    min_sample_rate: info.sample_rate,
                    max_sample_rate: info.sample_rate,
                    buffer_size: Some((info.period_frames, info.period_frames)),
                })
                .collect(),
        })
    }

    fn supports_concurrent_streams(&self) -> bool {
        // A hub runs many clients, and two hubs run side by side, so a
        // second stream can start while the first plays.
        true
    }

    fn start(
        &self,
        request: &BackendRequest,
        make: &mut dyn FnMut(BackendFormat) -> BackendCallback,
        status: StreamStatus,
    ) -> Result<BackendStream, DeviceError> {
        let hub = match (&request.input, &request.output) {
            (Some(input), Some(output)) if input.id != output.id => {
                return Err(DeviceError::UnsupportedSettings(format!(
                    "a stream records and plays through one hub, not {} and {}",
                    input.id, output.id
                )));
            }
            (_, Some(endpoint)) | (Some(endpoint), None) => hub_of(&endpoint.id)?,
            (None, None) => {
                return Err(DeviceError::UnsupportedSettings(String::from(
                    "neither an input nor an output was asked for",
                )));
            }
        };
        let info = mare_client::probe(&hub).map_err(|e| device_error(&hub, e))?;
        let side = |endpoint: &Option<BackendEndpoint>, direction| -> Result<u32, DeviceError> {
            let Some(endpoint) = endpoint else {
                return Ok(0);
            };
            if let Some(rate) = endpoint
                .settings
                .sample_rate
                .filter(|&r| r != info.sample_rate)
            {
                return Err(DeviceError::UnsupportedSettings(format!(
                    "the hub runs at {} Hz, not {rate} Hz",
                    info.sample_rate
                )));
            }
            Ok(endpoint
                .settings
                .channels
                .map_or_else(|| channels(&info, direction), u32::from))
        };
        let inputs = side(&request.input, Direction::Input)?;
        let outputs = side(&request.output, Direction::Output)?;
        let config = ClientConfig::new(&self.client_name, inputs, outputs);
        let client = Client::connect(&hub, &config).map_err(|e| device_error(&hub, e))?;
        let info = client.info();
        let callback = make(BackendFormat::new(
            info.sample_rate,
            inputs as usize,
            outputs as usize,
        ));
        let running = Running::start(client, callback, status)
            .map_err(|e| DeviceError::Other(Box::new(e)))?;

        let settings = |channels: u32| StreamSettings {
            sample_rate: Some(info.sample_rate),
            channels: Some(channels.min(u32::from(u16::MAX)) as u16),
            buffer_size: Some(info.period_frames),
        };
        let mut stream = BackendStream::new(running, info.sample_rate);
        if request.input.is_some() {
            stream = stream.with_input(Self::summary(&hub), settings(inputs));
        }
        if request.output.is_some() {
            stream = stream.with_output(Self::summary(&hub), settings(outputs));
        }
        Ok(stream)
    }
}

/// A client running the program's callback on a thread of its own, until
/// dropped.
struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    fn start(
        mut client: Client,
        mut callback: BackendCallback,
        status: StreamStatus,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("mare-client".into())
            .spawn(move || {
                let info = client.info();
                let format = callback.format();
                let max = info.max_frames as usize;
                let mut input = AudioBuffer::new(format.input_channels, max);
                let mut output = AudioBuffer::new(format.output_channels, max);
                let sample_rate = f64::from(info.sample_rate);
                let origin = Instant::now();
                let _promoted = mare_sys::promote_current_thread().ok();
                while !stopping.load(Ordering::Acquire) {
                    let ran = client.wait_cycle(Duration::from_millis(100), |cycle| {
                        let frames = cycle.frames();
                        input.set_num_frames(frames);
                        output.set_num_frames(frames);
                        for channel in 0..format.input_channels {
                            if let Some(samples) = cycle.input(channel as u32) {
                                input.channel_mut(channel).copy_from_slice(samples);
                            }
                        }
                        let timing = CallbackInfo {
                            frame: cycle.frame_time(),
                            sample_rate,
                            stream_time: origin.elapsed(),
                            ..CallbackInfo::default()
                        };
                        callback.process(&mut input, &mut output, &timing);
                        for channel in 0..format.output_channels {
                            if let Some(samples) = cycle.output(channel as u32) {
                                samples.copy_from_slice(output.channel(channel));
                            }
                        }
                    });
                    if ran.is_err() {
                        // The hub stopped or went away: the manager hears
                        // of it on its next poll and restarts the stream.
                        status.report_lost();
                        break;
                    }
                }
                // The client leaves and the callback goes back to the
                // manager as this thread ends.
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
