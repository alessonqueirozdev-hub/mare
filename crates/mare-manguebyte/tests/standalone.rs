//! A plugin played by Manguebyte's standalone, through a hub of MARÉ's own.
//!
//! This is how a standalone instrument (Caecilia's organ, for one) plays
//! through MARÉ: `Standalone::start_on` with a device manager on a
//! [`MareBackend`]. The hub runs on a fake device in real time, so nothing
//! opens a sound card, and the standalone listens to no MIDI port.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use manguebyte_core::AudioBuffer;
use manguebyte_devices::DeviceManager;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::param_set;
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_standalone::{MidiInputs, Standalone, StandaloneOptions, StandaloneSettings};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, HubConfig, SystemClock};
use mare_manguebyte::MareBackend;
use mare_sys::{HubName, current_pid};

struct NoParams {}
param_set!(NoParams {});

/// Blocks the instrument has played, across the test.
static BLOCKS: AtomicUsize = AtomicUsize::new(0);

/// An instrument that plays a steady 0.25 on both channels.
struct Steady(Arc<NoParams>);

impl Plugin for Steady {
    const INFO: PluginInfo = PluginInfo {
        id: "dev.mare.test.steady",
        name: "Steady",
        vendor: "MARÉ",
        version: "1",
        description: "plays 0.25",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = NoParams;

    fn new() -> Self {
        Self(Arc::new(NoParams {}))
    }

    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.0)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
        for channel in 0..buffer.num_channels() {
            buffer.channel_mut(channel).fill(0.25);
        }
        BLOCKS.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_standalone_plugin_plays_through_a_hub() {
    let name = HubName::new(&format!("s{:x}", current_pid() & 0xF_FFFF)).unwrap();
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 64,
            max_frames: 128,
            input_channels: 0,
            output_channels: 2,
        },
        Pacing::RealTime,
    );
    let hub = mare_hub::start(
        &HubConfig::new(name.clone()),
        device,
        Arc::new(SystemClock::new()),
    )
    .unwrap();

    let manager =
        DeviceManager::with_audio_backend(MareBackend::new("steady").with_hubs([name.clone()]));
    let options = StandaloneOptions {
        settings: StandaloneSettings {
            midi_inputs: MidiInputs::NONE,
            ..StandaloneSettings::default()
        },
        ..StandaloneOptions::default()
    };
    let app = Standalone::<Steady>::start_on(manager, options, None).unwrap();
    assert_eq!(app.sample_rate(), 48_000.0);
    assert!(
        app.device_name().contains(name.as_str()),
        "{}",
        app.device_name()
    );

    let start = Instant::now();
    loop {
        let stats = hub.stats();
        if stats.accepted == 1 && stats.client_cycles >= 300 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the plugin never played: {stats:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let stats = hub.stats();
    let (engine, error) = hub.stop();
    assert!(error.is_none());
    assert!(BLOCKS.load(Ordering::Relaxed) >= 300);
    eprintln!(
        "standalone: {} client cycles, {} late, device {:?}",
        stats.client_cycles,
        stats.late,
        app.device_name()
    );
    // The hub's device played the instrument, unless its last cycle was
    // late, which a loaded machine allows.
    if stats.late == 0 {
        for channel in 0..2 {
            let played = engine.device().last_output(channel);
            assert!(
                played.iter().all(|&x| (x - 0.25).abs() < 1e-6),
                "channel {channel}: {played:?}"
            );
        }
    }
    drop(app);
}
