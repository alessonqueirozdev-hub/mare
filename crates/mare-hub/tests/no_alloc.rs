//! Proves that the cycle path never touches the heap, on either side.
//!
//! The real-time rule (Manguebyte's ADR 0002, which MARÉ follows) says the
//! hub's cycle thread and a client's audio thread must not allocate or free.
//! This test replaces the global allocator with one that counts the calls
//! made by threads that are watching, and fails if a watched cycle makes
//! any: first with everything on one thread and a fake clock (late clients,
//! busy clients and events included), then with the hub and its clients on
//! their own threads, waiting on real doorbells.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{connect, fake_hub, unique_hub};
use mare_client::{Client, ClientConfig};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, HubConfig, SystemClock};
use mare_protocol::ring::UmpEvent;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

// SAFETY: every call is forwarded to the system allocator unchanged.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: forwarded with the caller's guarantees.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Self::record();
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `f` with this thread's heap calls counted, and returns the count.
fn heap_calls(f: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    f();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

fn note(time: u64) -> UmpEvent {
    UmpEvent {
        time,
        words: [0x4090_3C00, 0x8000_0000, 0, 0],
    }
}

fn process(cycle: &mut mare_client::Cycle<'_>) {
    while cycle.next_event().is_some() {}
    for channel in 0..cycle.output_channels() {
        if channel < cycle.input_channels() {
            let (input, output) = cycle.input_output(channel, channel).unwrap();
            for (out, x) in output.iter_mut().zip(input) {
                *out = 0.5 * x;
            }
        }
    }
    let time = cycle.frame_time();
    let _ = cycle.send_event(note(time));
}

#[test]
fn a_cycle_on_one_thread_never_allocates() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 16, |c, f| (c as f32) + f as f32);
    let mut clients: Vec<Client> = (0..8)
        .map(|i| {
            connect(
                &mut keeper,
                &hub,
                &ClientConfig::new(&format!("c{i}"), 2, 2),
            )
            .unwrap()
        })
        .collect();
    let ids: Vec<_> = engine.clients().collect();

    let mut run = |cycles: u64, engine: &mut mare_hub::Engine<FakeDevice>| {
        for n in 0..cycles {
            for &id in &ids {
                let _ = engine.send_event(id, note(n));
            }
            engine.begin_cycle().unwrap();
            for (i, client) in clients.iter_mut().enumerate() {
                // Client i skips one cycle in i + 2: late once, then busy.
                if (n + i as u64) % (i as u64 + 2) != 0 {
                    client.try_cycle(process).unwrap();
                }
            }
            engine.end_cycle().unwrap();
        }
    };
    run(5, &mut engine); // warm up
    let calls = heap_calls(|| run(500, &mut engine));
    assert_eq!(calls, 0, "the cycle allocated or freed {calls} times");
    let stats = engine.stats();
    assert!(stats.late > 0 && stats.busy > 0 && stats.events_received > 0);
}

#[test]
fn the_hub_and_its_clients_on_their_own_threads_never_allocate() {
    let hub = unique_hub();
    let config = HubConfig {
        max_clients: 8,
        ..HubConfig::new(hub.clone())
    };
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 128,
            max_frames: 128,
            input_channels: 2,
            output_channels: 2,
        },
        Pacing::RealTime,
    );
    let (mut engine, mut keeper) =
        mare_hub::create(&config, device, Arc::new(SystemClock::new())).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let keeper_thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                keeper.poll();
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let clients: Vec<_> = (0..4)
        .map(|i| {
            let hub = hub.clone();
            std::thread::spawn(move || {
                let mut client =
                    Client::connect(&hub, &ClientConfig::new(&format!("c{i}"), 2, 2)).unwrap();
                // Warm up outside the count.
                for _ in 0..5 {
                    let _ = client.wait_cycle(Duration::from_secs(5), process);
                }
                let mut ran = 0u32;
                let calls = heap_calls(|| {
                    while client
                        .wait_cycle(Duration::from_secs(5), |cycle| {
                            process(cycle);
                            ran += 1;
                        })
                        .is_ok()
                    {}
                });
                (calls, ran)
            })
        })
        .collect();
    while engine.clients().count() < 4 {
        engine.run_cycle().unwrap();
    }
    for _ in 0..20 {
        engine.run_cycle().unwrap();
    }
    let calls = heap_calls(|| {
        for _ in 0..400 {
            engine.run_cycle().unwrap();
        }
        engine.shut_down();
    });
    assert_eq!(calls, 0, "the hub's cycle allocated or freed {calls} times");
    for client in clients {
        let (calls, ran) = client.join().unwrap();
        assert_eq!(
            calls, 0,
            "a client's cycle allocated or freed {calls} times"
        );
        assert!(ran > 0);
    }
    stop.store(true, Ordering::Release);
    keeper_thread.join().unwrap();
}

#[test]
fn the_guard_sees_an_allocation() {
    let calls = heap_calls(|| drop(std::hint::black_box(vec![1u8; 16])));
    assert_eq!(calls, 2, "one allocation and one free");
}
