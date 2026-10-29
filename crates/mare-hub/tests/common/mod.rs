//! What the hub's tests share.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mare_client::{Client, ClientConfig, ConnectError};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{Clock, DeviceConfig, Engine, FakeClock, Housekeeper, HubConfig};
use mare_sys::{HubName, current_pid};

/// A hub name no other test, and no other run, uses at the same time.
pub fn unique_hub() -> HubName {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    HubName::new(&format!("h{:x}-{n}", current_pid() & 0xF_FFFF)).unwrap()
}

/// 48 kHz, 64-frame periods (1.33 ms), two channels each way.
pub const DEVICE: DeviceConfig = DeviceConfig {
    sample_rate: 48_000,
    period_frames: 64,
    max_frames: 128,
    input_channels: 2,
    output_channels: 2,
};

/// A hub on a fake device and a fake clock, for a test to drive by hand.
pub fn fake_hub(
    name: &HubName,
    max_clients: u32,
    input: mare_hub::fake::InputFn,
) -> (Engine<FakeDevice>, Housekeeper, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new());
    let device = FakeDevice::new(DEVICE, Pacing::Fake(Arc::clone(&clock))).with_input(input);
    let config = HubConfig {
        max_clients,
        ..HubConfig::new(name.clone())
    };
    let (engine, keeper) =
        mare_hub::create(&config, device, Arc::clone(&clock) as Arc<dyn Clock>).unwrap();
    (engine, keeper, clock)
}

/// Connects a client while this thread answers for the hub's housekeeper.
pub fn connect(
    keeper: &mut Housekeeper,
    hub: &HubName,
    config: &ClientConfig,
) -> Result<Client, ConnectError> {
    std::thread::scope(|scope| {
        let connecting = scope.spawn(|| Client::connect(hub, config));
        while !connecting.is_finished() {
            keeper.poll();
            std::thread::yield_now();
        }
        connecting.join().unwrap()
    })
}
