//! A client in another process, killed in the middle of a cycle.
//!
//! The child is this test binary again, running only `child_client`, which
//! does nothing unless the parent names a hub for it in the environment.

mod common;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::unique_hub;
use mare_client::{Client, ClientConfig, Stopped};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, HubConfig, RunningHub, Stats, SystemClock};
use mare_protocol::registry::{RegistryHeader, RegistryLayout, RegistryView, slot_state};
use mare_sys::{HubName, SharedMemory};

const CHILD_HUB: &str = "MARE_TEST_CHILD_HUB";

/// How many cycles the child runs before it hangs inside one.
const CHILD_CYCLES: u32 = 50;

#[test]
#[ignore = "started by a_client_killed_mid_cycle_is_detected_and_freed, in a child process"]
fn child_client() {
    let Ok(name) = std::env::var(CHILD_HUB) else {
        return;
    };
    let hub = HubName::new(&name).unwrap();
    let mut client = Client::connect(&hub, &ClientConfig::new("child", 0, 2)).unwrap();
    let mut ran = 0;
    client.run(|cycle| {
        cycle.output(0).unwrap().fill(1.0);
        ran += 1;
        if ran == CHILD_CYCLES {
            // Hang mid-cycle, outputs half written, until killed.
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

fn wait_for(hub: &RunningHub<FakeDevice>, what: &str, done: impl Fn(&Stats) -> bool) -> Stats {
    let start = Instant::now();
    loop {
        let stats = hub.stats();
        if done(&stats) {
            return stats;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}: {stats:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_client_killed_mid_cycle_is_detected_and_freed() {
    let name = unique_hub();
    let config = HubConfig {
        max_clients: 4,
        ..HubConfig::new(name.clone())
    };
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 128,
            max_frames: 128,
            input_channels: 0,
            output_channels: 2,
        },
        Pacing::RealTime,
    );
    let hub = mare_hub::start(&config, device, Arc::new(SystemClock::new())).unwrap();

    // A client in this process, in slot 0, to show the others carry on.
    let steady = {
        let name = name.clone();
        std::thread::spawn(move || {
            let mut client = Client::connect(&name, &ClientConfig::new("steady", 0, 2)).unwrap();
            assert_eq!(client.slot(), 0);
            client.run(|cycle| cycle.output(1).unwrap().fill(0.5))
        })
    };
    wait_for(&hub, "the steady client", |s| s.accepted == 1);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child_client", "--exact", "--ignored", "--nocapture"])
        .env(CHILD_HUB, name.as_str())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&hub, "the child", |s| s.accepted == 2);
    // The child hangs inside its 50th cycle, so the hub soon finds it busy.
    let hung = wait_for(&hub, "the child to hang", |s| s.busy >= 20);
    assert_eq!(hung.died, 0);

    child.kill().unwrap();
    child.wait().unwrap();
    let killed = Instant::now();
    let freed = wait_for(&hub, "the child to be found dead", |s| s.died == 1);
    let noticed = killed.elapsed();

    // Its slot is free again, under a new generation.
    let registry = SharedMemory::open(&name.registry()).unwrap();
    let len = registry.len() as u64;
    // SAFETY: the mapping is kept to the end of the test.
    let header = unsafe { RegistryHeader::read_from(registry.base(), len) }.unwrap();
    let layout = RegistryLayout::validate(&header, len).unwrap();
    // SAFETY: as above.
    let view = unsafe { RegistryView::from_raw(registry.base(), len, layout) }.unwrap();
    let slot = view.slot(1).unwrap();
    assert_eq!(slot.state.load(Ordering::Acquire), slot_state::FREE);
    assert_eq!(slot.generation.load(Ordering::Acquire), 1);
    assert_eq!(
        view.slot(0).unwrap().state.load(Ordering::Acquire),
        slot_state::ACCEPTED
    );

    // The hub went on cycling throughout, and the steady client was heard.
    let after = wait_for(&hub, "more cycles", |s| s.cycles >= freed.cycles + 50);
    assert!(after.client_cycles > hung.client_cycles);
    let (engine, error) = hub.stop();
    assert!(error.is_none());
    drop(engine);
    assert_eq!(steady.join().unwrap(), Stopped::HubStopped);
    // The housekeeper looks every 2 ms; allow for a loaded machine.
    assert!(noticed < Duration::from_secs(5), "took {noticed:?}");
}

const CHILD_HUB_NAME: &str = "MARE_TEST_CHILD_HUB_NAME";

#[test]
#[ignore = "started by a_client_learns_at_once_that_its_hub_died, in a child process"]
fn child_hub() {
    let Ok(name) = std::env::var(CHILD_HUB_NAME) else {
        return;
    };
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 128,
            max_frames: 128,
            input_channels: 0,
            output_channels: 2,
        },
        Pacing::RealTime,
    );
    let config = HubConfig::new(HubName::new(&name).unwrap());
    let _hub = mare_hub::start(&config, device, Arc::new(SystemClock::new())).unwrap();
    // Run until killed.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn a_client_learns_at_once_that_its_hub_died() {
    let name = unique_hub();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child_hub", "--exact", "--ignored", "--nocapture"])
        .env(CHILD_HUB_NAME, name.as_str())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // The child's hub takes a moment to publish its registry.
    let start = Instant::now();
    let mut client = loop {
        match Client::connect(&name, &ClientConfig::new("orphan", 0, 2)) {
            Ok(client) => break client,
            Err(_) if start.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("never connected: {error}"),
        }
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    let listener = std::thread::spawn(move || {
        let mut ran = 0u32;
        let stopped = client.run(|cycle| {
            cycle.output(0).unwrap().fill(0.0);
            ran += 1;
            if ran == 10 {
                let _ = sender.send(());
            }
        });
        (stopped, Instant::now())
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("the client never ran");

    child.kill().unwrap();
    let killed = Instant::now();
    child.wait().unwrap();
    let (stopped, noticed) = listener.join().unwrap();
    let took = noticed.saturating_duration_since(killed);
    eprintln!("the client noticed in {took:?}");
    assert_eq!(stopped, Stopped::HubGone);
    // On Windows the client's wait ends on the hub's process handle; the
    // bound leaves room for a loaded machine. Elsewhere the client learns
    // at its next timeout, 100 ms at most.
    let bound = if cfg!(windows) {
        Duration::from_millis(50)
    } else {
        Duration::from_secs(1)
    };
    assert!(took < bound, "took {took:?}");
}
