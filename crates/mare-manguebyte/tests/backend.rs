//! The backend behind a Manguebyte device manager, on hubs of MARÉ's own.
//!
//! Every hub here runs on a fake device in real time: nothing opens a sound
//! card. A device manager lists the hubs as devices, plays through one as a
//! program built on Manguebyte would, and hears of it when the hub stops.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use manguebyte_devices::{
    AudioBackend, BackendCallback, BackendEndpoint, BackendRequest, DeviceChoice, DeviceError,
    DeviceEvent, DeviceIo, DeviceManager, DeviceSetup, Direction, StreamSettings, StreamStatus,
};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, DeviceLatency, HubConfig, RunningHub, Stats, SystemClock};
use mare_manguebyte::{HOST_NAME, MareBackend, device_id};
use mare_sys::{HubName, current_pid};

fn unique_hub() -> HubName {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    HubName::new(&format!("m{:x}-{n}", current_pid() & 0xF_FFFF)).unwrap()
}

/// 0.25 on channel 0, 0.5 on channel 1, and so on.
fn levels(channel: u32, _frame: u64) -> f32 {
    0.25 * (channel + 1) as f32
}

/// What the hubs' devices report.
const LATENCY: DeviceLatency = DeviceLatency {
    input_frames: 96,
    output_frames: 160,
};

fn hub(name: &HubName, inputs: u32, outputs: u32) -> RunningHub<FakeDevice> {
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 64,
            max_frames: 128,
            input_channels: inputs,
            output_channels: outputs,
        },
        Pacing::RealTime,
    )
    .with_input(levels)
    .with_latency(LATENCY);
    mare_hub::start(
        &HubConfig::new(name.clone()),
        device,
        Arc::new(SystemClock::new()),
    )
    .unwrap()
}

fn wait_for(hub: &RunningHub<FakeDevice>, done: impl Fn(&Stats) -> bool) -> Stats {
    let start = Instant::now();
    loop {
        let stats = hub.stats();
        if done(&stats) {
            return stats;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out: {stats:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn running_hubs_are_devices_that_say_how_they_run() {
    let (duplex, player, missing) = (unique_hub(), unique_hub(), unique_hub());
    let duplex_hub = hub(&duplex, 2, 2);
    let player_hub = hub(&player, 0, 4);
    let backend =
        MareBackend::new("lister").with_hubs([missing.clone(), duplex.clone(), player.clone()]);
    assert_eq!(backend.host_name(), HOST_NAME);

    let snapshot = backend.snapshot().unwrap();
    let ids = |devices: &[manguebyte_devices::DeviceSummary]| {
        devices.iter().map(|d| d.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&snapshot.outputs),
        [device_id(&duplex), device_id(&player)]
    );
    assert_eq!(ids(&snapshot.inputs), [device_id(&duplex)]);
    assert_eq!(snapshot.default_output, Some(device_id(&duplex)));
    assert_eq!(snapshot.default_input, Some(device_id(&duplex)));

    let info = backend
        .device_info(Direction::Output, &device_id(&player))
        .unwrap();
    assert_eq!(info.default_sample_rate, Some(48_000));
    assert_eq!(info.default_channels, Some(4));
    assert_eq!(info.configs.len(), 4);
    assert!(info.configs.iter().all(|c| c.min_sample_rate == 48_000
        && c.max_sample_rate == 48_000
        && c.buffer_size == Some((64, 64))));
    assert!(!info.is_default, "the first hub listed is the default");
    assert!(matches!(
        backend.device_info(Direction::Output, &device_id(&missing)),
        Err(DeviceError::DeviceNotFound(_))
    ));
    assert!(matches!(
        backend.device_info(Direction::Output, "not a hub"),
        Err(DeviceError::DeviceNotFound(_))
    ));

    // A hub that stops is no longer listed.
    drop(player_hub.stop());
    let snapshot = backend.snapshot().unwrap();
    assert_eq!(ids(&snapshot.outputs), [device_id(&duplex)]);
    drop(duplex_hub.stop());
    assert_eq!(backend.snapshot().unwrap().outputs, []);
}

#[test]
fn a_device_manager_plays_through_a_hub_and_hears_when_it_stops() {
    let name = unique_hub();
    let hub = hub(&name, 2, 2);
    let mut manager =
        DeviceManager::with_audio_backend(MareBackend::new("manager").with_hubs([name.clone()]));
    assert_eq!(manager.host_name(), HOST_NAME);

    let wrong = Arc::new(AtomicUsize::new(0));
    let blocks = Arc::new(AtomicUsize::new(0));
    // The latencies the program was told, in frames, as f64 bits.
    let heard = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let (wrong_in, blocks_in, heard_in) =
        (Arc::clone(&wrong), Arc::clone(&blocks), Arc::clone(&heard));
    let report = manager
        .start(
            &DeviceSetup::default().with_input(DeviceChoice::Default),
            move |io: &mut DeviceIo<'_>| {
                // Every input sample as the hub's device recorded it, and
                // twice it out.
                for channel in 0..io.output.num_channels() {
                    let expected = 0.25 * (channel + 1) as f32;
                    let input = io.input.channel(channel);
                    if input.iter().any(|&x| x != expected) {
                        wrong_in.fetch_add(1, Ordering::Relaxed);
                    }
                    for (out, x) in io.output.channel_mut(channel).iter_mut().zip(input) {
                        *out = 2.0 * x;
                    }
                }
                blocks_in.fetch_add(1, Ordering::Relaxed);
                let frames = [
                    io.info.input_latency_frames(),
                    io.info.output_latency_frames(),
                ];
                for (store, frames) in heard_in.iter().zip(frames) {
                    store.store(frames.unwrap_or(-1.0).to_bits(), Ordering::Relaxed);
                }
            },
        )
        .unwrap();
    assert!(report.is_exact(), "{:?}", report.fallbacks);
    assert_eq!(manager.format().unwrap().sample_rate, 48_000.0);
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        device_id(&name)
    );
    assert_eq!(
        manager
            .running_settings(Direction::Output)
            .unwrap()
            .buffer_size,
        Some(64)
    );

    let stats = wait_for(&hub, |s| s.accepted == 1 && s.client_cycles >= 300);
    let (engine, error) = hub.stop();
    assert!(error.is_none());
    assert_eq!(wrong.load(Ordering::Relaxed), 0);
    assert!(blocks.load(Ordering::Relaxed) >= 300);
    // The program heard the device's latencies: the output's as the hub
    // published it, the input's at least as long (the device manager may
    // add a reserve of its own between input and output). A `Duration`
    // keeps whole nanoseconds, a few millionths of a frame.
    let [input, output] = [0, 1].map(|i| f64::from_bits(heard[i].load(Ordering::Relaxed)));
    assert!((output - 160.0).abs() < 1e-3, "output latency {output}");
    assert!(input >= 96.0 - 1e-3, "input latency {input}");
    // The last cycle's mix is the program's output, unless it was late for
    // it, which a loaded machine allows.
    if stats.late == 0 {
        assert!(engine.device().last_output(0).iter().all(|&x| x == 0.5));
        assert!(engine.device().last_output(1).iter().all(|&x| x == 1.0));
    }

    // The hub stopped under the stream: the manager hears of it.
    let start = Instant::now();
    loop {
        let events = manager.poll_devices();
        if events
            .iter()
            .any(|e| matches!(e, DeviceEvent::StreamLost { .. }))
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the loss was never reported"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    manager.stop();
}

#[test]
fn what_a_hub_cannot_serve_is_refused() {
    let (one, other, missing) = (unique_hub(), unique_hub(), unique_hub());
    let one_hub = hub(&one, 2, 2);
    let other_hub = hub(&other, 2, 2);
    let backend = MareBackend::new("refused").with_hubs([one.clone(), other.clone()]);
    let start = |request: BackendRequest| {
        backend.start(
            &request,
            &mut |format| BackendCallback::from_fn(format, 256, |_, _, _| {}),
            StreamStatus::new(|_| {}),
        )
    };
    let endpoint = |hub: &HubName, settings: StreamSettings| {
        Some(BackendEndpoint::new(device_id(hub), settings))
    };

    let at_44k = endpoint(&one, StreamSettings::default().with_sample_rate(44_100));
    assert!(matches!(
        start(BackendRequest::new(None, at_44k, 256)),
        Err(DeviceError::UnsupportedSettings(_))
    ));
    let mixed = BackendRequest::new(
        endpoint(&one, StreamSettings::default()),
        endpoint(&other, StreamSettings::default()),
        256,
    );
    assert!(matches!(
        start(mixed),
        Err(DeviceError::UnsupportedSettings(_))
    ));
    let gone = endpoint(&missing, StreamSettings::default());
    assert!(matches!(
        start(BackendRequest::new(None, gone, 256)),
        Err(DeviceError::DeviceNotFound(_))
    ));
    // What it can serve starts, and stops when dropped.
    let stream = start(BackendRequest::new(
        None,
        endpoint(&one, StreamSettings::default().with_channels(6)),
        256,
    ))
    .unwrap();
    assert_eq!(stream.sample_rate(), 48_000);
    let (_, settings) = stream.endpoint(Direction::Output).unwrap();
    assert_eq!(settings.channels, Some(6));
    wait_for(&one_hub, |s| s.accepted == 1 && s.client_cycles > 10);
    drop(stream);
    wait_for(&one_hub, |s| s.left == 1);
    drop((one_hub.stop(), other_hub.stop()));
}
