//! A real WASAPI device driven by the hub, playing silence.
//!
//! Rules for these tests, which run on whatever machine runs them:
//!
//! - they play nothing but silence;
//! - they never open a Behringer interface (the development machine's
//!   UMC404HD): any device whose name contains "UMC" or "Behringer" is
//!   skipped, whatever was asked;
//! - shared mode runs by default, since other programs keep playing;
//!   exclusive mode takes the device from them, so its test is ignored and
//!   run by hand (`cargo test -p mare-wasapi -- --ignored`);
//! - with no device (a CI runner), they say so and pass.
//!
//! `MARE_TEST_DEVICE` picks a device by part of its name; without it, the
//! default output is used.

#![cfg(windows)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mare_client::{Client, ClientConfig};
use mare_hub::{Clock, Device, HubConfig, SystemClock};
use mare_sys::{HubName, current_pid};
use mare_wasapi::{Endpoint, Mode, OpenOptions, WasapiDevice, render_endpoints};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
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

fn heap_calls(f: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    f();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

/// Whether a test may never open this device.
fn forbidden(name: &str) -> bool {
    let name = name.to_uppercase();
    name.contains("UMC") || name.contains("BEHRINGER")
}

/// The device a test may open, or `None` (said on stderr) if there is none.
fn test_endpoint() -> Option<Endpoint> {
    let endpoints = match render_endpoints() {
        Ok(endpoints) => endpoints,
        Err(error) => {
            eprintln!("skipped: cannot list audio devices: {error}");
            return None;
        }
    };
    let chosen = match std::env::var("MARE_TEST_DEVICE") {
        Ok(part) => {
            let part = part.to_lowercase();
            endpoints
                .into_iter()
                .find(|e| e.name.to_lowercase().contains(&part))
        }
        Err(_) => endpoints.into_iter().find(|e| e.is_default),
    };
    match chosen {
        None => {
            eprintln!("skipped: no audio output to test with");
            None
        }
        Some(endpoint) if forbidden(&endpoint.name) => {
            eprintln!("skipped: {} is never opened by a test", endpoint.name);
            None
        }
        Some(endpoint) => Some(endpoint),
    }
}

fn unique_hub() -> HubName {
    HubName::new(&format!("w{:x}", current_pid() & 0xFF_FFFF)).unwrap()
}

#[test]
fn the_rule_recognises_the_interfaces_tests_must_never_open() {
    assert!(forbidden("Line (UMC404HD 192k)"));
    assert!(forbidden("Alto-falantes (2- UMC404HD 192k)"));
    assert!(forbidden("BEHRINGER UMC 404HD"));
    assert!(!forbidden("Alto-falantes (2- Realtek(R) Audio)"));
}

#[test]
fn endpoints_can_be_listed_without_opening_any() {
    // A CI runner may have no audio service at all.
    let endpoints = match render_endpoints() {
        Ok(endpoints) => endpoints,
        Err(error) => {
            eprintln!("skipped: cannot list audio devices: {error}");
            return;
        }
    };
    assert!(endpoints.iter().filter(|e| e.is_default).count() <= 1);
    for endpoint in &endpoints {
        assert!(!endpoint.id.is_empty());
        eprintln!(
            "{} {}",
            endpoint.name,
            if endpoint.is_default { "(default)" } else { "" }
        );
    }
}

/// Opens `endpoint` in `mode`, runs the hub on it with one client writing
/// silence for about a second, and checks the timing and the heap.
fn plays_silence(endpoint: &Endpoint, mode: Mode) {
    let device = WasapiDevice::open(&endpoint.id, &OpenOptions::new(mode)).unwrap();
    let config = device.config();
    let latency = device.latency();
    eprintln!(
        "{}: {:?}, {:?}, {} Hz, {} channels, {:?}",
        device.name(),
        mode,
        device.sample_format(),
        config.sample_rate,
        config.output_channels,
        latency
    );
    assert!(config.period_frames > 0 && config.period_frames <= config.max_frames);
    let period =
        Duration::from_secs_f64(f64::from(config.period_frames) / f64::from(config.sample_rate));

    let hub = unique_hub();
    let (mut engine, mut keeper) = mare_hub::create(
        &HubConfig::new(hub.clone()),
        device,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
    )
    .unwrap();
    // A client that writes silence, on its own thread.
    let client = std::thread::spawn(move || {
        let mut client = Client::connect(&hub, &ClientConfig::new("silence", 0, 2)).unwrap();
        client.run(|cycle| {
            for channel in 0..cycle.output_channels() {
                cycle.output(channel).unwrap().fill(0.0);
            }
        })
    });
    let start = Instant::now();
    while engine.clients().count() == 0 {
        keeper.poll();
        engine.run_cycle().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the client never connected"
        );
    }
    let _promoted = mare_sys::promote_current_thread();
    for _ in 0..20 {
        engine.run_cycle().unwrap();
    }

    let cycles = (Duration::from_secs(1).as_secs_f64() / period.as_secs_f64()) as u32;
    let before = engine.stats();
    let started = Instant::now();
    let mut frames = 0usize;
    let calls = heap_calls(|| {
        for _ in 0..cycles {
            let report = engine.run_cycle().unwrap();
            frames += report.frames;
        }
    });
    let took = started.elapsed();
    let stats = engine.stats();
    eprintln!(
        "measured: {cycles} cycles, {frames} frames in {took:?}; the client was late {} and busy {} times in them",
        stats.late - before.late,
        stats.busy - before.busy
    );
    eprintln!(
        "before: {before:?}
after: {stats:?}"
    );
    engine.shut_down();
    drop(engine);
    client.join().unwrap();

    assert_eq!(calls, 0, "the cycle allocated or freed {calls} times");
    // The device sets the pace: the frames played take as long as they last,
    // give or take the buffer that was filled ahead.
    let played = Duration::from_secs_f64(frames as f64 / f64::from(config.sample_rate));
    let ahead =
        Duration::from_secs_f64(f64::from(latency.buffer_frames) / f64::from(config.sample_rate));
    assert!(
        took + ahead + Duration::from_millis(50) >= played,
        "{took:?} for {played:?}"
    );
    assert!(
        took <= played + Duration::from_millis(500),
        "{took:?} for {played:?}"
    );
    assert!(stats.client_cycles > 0);
}

#[test]
fn the_hub_plays_silence_through_a_shared_device() {
    if let Some(endpoint) = test_endpoint() {
        plays_silence(&endpoint, Mode::Shared);
    }
}

#[test]
#[ignore = "takes the device from every other program; run by hand"]
fn the_hub_plays_silence_through_an_exclusive_device() {
    if let Some(endpoint) = test_endpoint() {
        plays_silence(&endpoint, Mode::Exclusive);
    }
}

/// The input a duplex test may open, chosen like `test_endpoint` but among
/// inputs, by `MARE_TEST_INPUT`.
fn test_input() -> Option<Endpoint> {
    let endpoints = mare_wasapi::capture_endpoints().ok()?;
    let chosen = match std::env::var("MARE_TEST_INPUT") {
        Ok(part) => {
            let part = part.to_lowercase();
            endpoints
                .into_iter()
                .find(|e| e.name.to_lowercase().contains(&part))
        }
        Err(_) => endpoints.into_iter().find(|e| e.is_default),
    };
    match chosen {
        None => {
            eprintln!("skipped: no audio input to test with");
            None
        }
        Some(endpoint) if forbidden(&endpoint.name) => {
            eprintln!("skipped: {} is never opened by a test", endpoint.name);
            None
        }
        Some(endpoint) => Some(endpoint),
    }
}

/// Opens an output and an input together in `mode`, runs the hub with a
/// client that reads the input and writes silence for about two seconds,
/// and checks that the input kept up with the output. What the input
/// records is looked at only to check it is finite, and is kept nowhere.
fn duplex(output: &Endpoint, input: &Endpoint, mode: Mode) {
    let device = WasapiDevice::open_duplex(&output.id, &input.id, &OpenOptions::new(mode)).unwrap();
    let config = device.config();
    eprintln!(
        "{} + {}: {:?}, out {:?}, in {:?}, {} Hz, {} in, {} out, {:?}",
        device.name(),
        device.input_name().unwrap(),
        mode,
        device.sample_format(),
        device.input_sample_format().unwrap(),
        config.sample_rate,
        config.input_channels,
        config.output_channels,
        device.latency()
    );
    assert!(config.input_channels > 0);
    let period = f64::from(config.period_frames) / f64::from(config.sample_rate);

    let hub = unique_hub();
    let (mut engine, mut keeper) = mare_hub::create(
        &HubConfig::new(hub.clone()),
        device,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
    )
    .unwrap();
    let inputs = config.input_channels;
    let client = std::thread::spawn(move || {
        let mut client = Client::connect(&hub, &ClientConfig::new("listen", inputs, 2)).unwrap();
        let mut finite = true;
        let stopped = client.run(|cycle| {
            for channel in 0..cycle.input_channels() {
                finite &= cycle.input(channel).unwrap().iter().all(|x| x.is_finite());
            }
            for channel in 0..cycle.output_channels() {
                cycle.output(channel).unwrap().fill(0.0);
            }
        });
        (stopped, finite)
    });
    let start = Instant::now();
    while engine.clients().count() == 0 {
        keeper.poll();
        engine.run_cycle().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the client never connected"
        );
    }
    let _promoted = mare_sys::promote_current_thread();
    // Half a second for both streams and the client to settle.
    for _ in 0..(0.5 / period) as u32 {
        engine.run_cycle().unwrap();
    }
    let before = engine.device().input_stats().unwrap();
    let hub_before = engine.stats();
    let cycles = (2.0 / period) as u32;
    let calls = heap_calls(|| {
        for _ in 0..cycles {
            engine.run_cycle().unwrap();
        }
    });
    let after = engine.device().input_stats().unwrap();
    let backlog = engine.device().input_backlog().unwrap();
    let input_buffer = engine.device().input_buffer_frames().unwrap();
    let stats = engine.stats();
    engine.shut_down();
    drop(engine);
    let (_, finite) = client.join().unwrap();

    eprintln!(
        "input buffer {} frames, largest packet {}; positions jumped {} times, {} frames repeated, {} skipped",
        input_buffer,
        after.largest_packet,
        after.position_jumps - before.position_jumps,
        after.repeated_frames - before.repeated_frames,
        after.skipped_frames - before.skipped_frames,
    );
    let frames = u64::from(cycles) * u64::from(config.period_frames);
    let padded = after.padded - before.padded;
    let dropped = after.dropped - before.dropped;
    eprintln!(
        "measured: {cycles} cycles, {frames} frames out; input delivered {}, padded {padded} in {} short cycles, dropped {dropped}, discontinuities {}, backlog at the end {} frames; client late {} busy {}",
        after.frames_in - before.frames_in,
        after.short_cycles - before.short_cycles,
        after.discontinuities - before.discontinuities,
        backlog,
        stats.late - hub_before.late,
        stats.busy - hub_before.busy,
    );
    assert_eq!(calls, 0, "the cycle allocated or freed {calls} times");
    assert!(finite, "the client read something that is not a number");
    // Over two seconds the input must deliver what the output plays, give or
    // take a buffer at either end.
    let slack = 2 * u64::from(config.max_frames);
    assert!(padded <= slack, "padded {padded} of {frames}");
    assert!(dropped <= slack, "dropped {dropped} of {frames}");
}

#[test]
#[ignore = "opens a microphone; run by hand"]
fn the_hub_reads_an_input_beside_a_shared_output() {
    if let (Some(output), Some(input)) = (test_endpoint(), test_input()) {
        duplex(&output, &input, Mode::Shared);
    }
}

#[test]
#[ignore = "opens a microphone and takes the devices from other programs; run by hand"]
fn the_hub_reads_an_input_beside_an_exclusive_output() {
    if let (Some(output), Some(input)) = (test_endpoint(), test_input()) {
        duplex(&output, &input, Mode::Exclusive);
    }
}
