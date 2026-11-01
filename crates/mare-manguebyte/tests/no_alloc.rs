//! Proves that the backend's cycle thread never touches the heap.
//!
//! The program's callback, the first time it runs, starts counting every
//! allocation and free made on its thread, the client's cycle thread: from
//! then on the waits for the hub, the copies in and out and Manguebyte's
//! callback layer are all counted, cycle after cycle.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use manguebyte_devices::{DeviceChoice, DeviceIo, DeviceManager, DeviceSetup};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, HubConfig, SystemClock};
use mare_manguebyte::MareBackend;
use mare_sys::{HubName, current_pid};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

static HEAP_CALLS: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                HEAP_CALLS.fetch_add(1, Ordering::Relaxed);
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

#[test]
fn the_cycle_thread_never_allocates() {
    let name = HubName::new(&format!("n{:x}", current_pid() & 0xF_FFFF)).unwrap();
    let device = FakeDevice::new(
        DeviceConfig {
            sample_rate: 48_000,
            period_frames: 64,
            max_frames: 128,
            input_channels: 2,
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
    let mut manager =
        DeviceManager::with_audio_backend(MareBackend::new("counted").with_hubs([name]));
    let blocks = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&blocks);
    manager
        .start(
            &DeviceSetup::default().with_input(DeviceChoice::Default),
            move |io: &mut DeviceIo<'_>| {
                WATCHING.with(|w| w.set(true));
                for channel in 0..io.output.num_channels() {
                    io.output.channel_mut(channel).fill(0.1);
                }
                counted.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
    let start = Instant::now();
    while blocks.load(Ordering::Relaxed) < 500 {
        assert!(start.elapsed() < Duration::from_secs(60), "it never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    let calls = HEAP_CALLS.load(Ordering::Relaxed);
    manager.stop();
    drop(hub.stop());
    assert_eq!(calls, 0, "the cycle thread allocated or freed");

    // The guard itself sees an allocation and a free. (The cycle thread
    // freed its buffers as it ended, after the count above.)
    let before = HEAP_CALLS.load(Ordering::Relaxed);
    WATCHING.with(|w| w.set(true));
    drop(std::hint::black_box(vec![1u8; 16]));
    WATCHING.with(|w| w.set(false));
    assert_eq!(HEAP_CALLS.load(Ordering::Relaxed) - before, 2);
}
