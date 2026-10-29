//! The hub on its own threads, with clients on theirs, in real time.
//!
//! These run on whatever machine runs the tests, loaded or not, so they
//! check what must hold however late threads are scheduled: the hub keeps
//! cycling, never waits past the budget by more than scheduling noise, and
//! every client that keeps up is heard. The exact accounting of late clients
//! is in `cycles.rs`, on a fake clock.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::unique_hub;
use mare_client::{Client, ClientConfig, Stopped};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, HubConfig, RunningHub, Stats, SystemClock};

/// What the scheduler may add to a wait, on a loaded test machine.
///
/// The cycle thread runs at normal priority until it is registered with
/// MMCSS (milestone 3), so any runnable thread can delay it by a scheduler
/// quantum, 15 to 30 ms on desktop Windows. With a hundred client threads
/// waking at once on eight logical cores, one wait of 21.8 ms was measured
/// on the development laptop (budget 3.7 ms). That is preemption, not the
/// hub waiting: `cycles.rs` proves on a fake clock that it never waits past
/// the deadline by choice.
const SLACK: Duration = Duration::from_millis(100);

fn device(period_frames: u32) -> DeviceConfig {
    DeviceConfig {
        sample_rate: 48_000,
        period_frames,
        max_frames: period_frames,
        input_channels: 2,
        output_channels: 2,
    }
}

fn start(name: &mare_sys::HubName, period_frames: u32, max_clients: u32) -> RunningHub<FakeDevice> {
    let config = HubConfig {
        max_clients,
        ..HubConfig::new(name.clone())
    };
    let device = FakeDevice::new(device(period_frames), Pacing::RealTime);
    mare_hub::start(&config, device, Arc::new(SystemClock::new())).unwrap()
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
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn budget(period_frames: u32) -> Duration {
    Duration::from_nanos(u64::from(period_frames) * 1_000_000_000 / 48_000).mul_f64(0.7)
}

#[test]
fn a_hundred_clients_on_their_own_threads() {
    let name = unique_hub();
    let hub = start(&name, 256, 128);
    let clients: Vec<_> = (0..100)
        .map(|i| {
            let name = name.clone();
            std::thread::spawn(move || {
                let mut client =
                    Client::connect(&name, &ClientConfig::new(&format!("c{i}"), 2, 2)).unwrap();
                let mut ran = 0u64;
                let stopped = client.run(|cycle| {
                    cycle.output(0).unwrap().fill(0.01);
                    ran += 1;
                });
                (stopped, ran)
            })
        })
        .collect();
    let joined = wait_for(&hub, "100 clients", |s| s.accepted == 100);
    wait_for(&hub, "200 cycles with everyone", |s| {
        s.cycles >= joined.cycles + 200
    });
    let (engine, error) = hub.stop();
    assert!(error.is_none());
    let stats = engine.stats();
    drop(engine);
    for client in clients {
        let (stopped, ran) = client.join().unwrap();
        assert_eq!(stopped, Stopped::HubStopped);
        assert!(ran > 0);
    }
    assert!(stats.client_cycles > 0);
    assert!(
        Duration::from_nanos(stats.longest_wait_ns) <= budget(256) + SLACK,
        "{stats:?}"
    );
}

#[test]
fn a_stalled_client_does_not_hold_the_others() {
    let name = unique_hub();
    let hub = start(&name, 128, 8);
    let release = Arc::new(AtomicBool::new(false));

    let stalled = {
        let (name, release) = (name.clone(), Arc::clone(&release));
        std::thread::spawn(move || {
            let mut client = Client::connect(&name, &ClientConfig::new("stalled", 0, 2)).unwrap();
            client.run(|cycle| {
                cycle.output(1).unwrap().fill(1.0);
                // Each cycle takes 50 ms, far past a 2.7 ms period.
                let until = Instant::now() + Duration::from_millis(50);
                while Instant::now() < until && !release.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        })
    };
    wait_for(&hub, "the stalled client", |s| s.accepted == 1);
    let steady = {
        let name = name.clone();
        std::thread::spawn(move || {
            let mut client = Client::connect(&name, &ClientConfig::new("steady", 0, 2)).unwrap();
            client.run(|cycle| cycle.output(0).unwrap().fill(0.5));
            client.late_cycles()
        })
    };
    let joined = wait_for(&hub, "the steady client", |s| s.accepted == 2);
    let done = wait_for(&hub, "300 cycles", |s| s.cycles >= joined.cycles + 300);
    release.store(true, Ordering::Release);
    let (engine, _) = hub.stop();
    drop(engine);
    stalled.join().unwrap();
    steady.join().unwrap();

    let cycles = done.cycles - joined.cycles;
    // The stalled client misses nearly every cycle; it is woken at most once
    // per 50 ms, and is busy the rest of the time.
    let missed = (done.late + done.busy) - (joined.late + joined.busy);
    assert!(missed >= cycles / 2, "{done:?}");
    // The steady client keeps going. On an idle machine it misses none;
    // the bound leaves room for a loaded one.
    let steady_on_time = done.client_cycles - joined.client_cycles;
    assert!(
        steady_on_time >= cycles / 2,
        "{steady_on_time} of {cycles}: {done:?}"
    );
    assert!(
        Duration::from_nanos(done.longest_wait_ns) <= budget(128) + SLACK,
        "{done:?}"
    );
}
