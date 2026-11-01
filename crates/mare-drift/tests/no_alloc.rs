//! Proves that resampling, controlling and a link never touch the heap.
//!
//! The hub's cycle thread runs them, so, as everywhere on that thread, they
//! must not allocate or free (Manguebyte's ADR 0002). A counting allocator
//! fails this test if any `push`, `pull`, `set_ratio` or `update` does.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use mare_drift::{ControllerSettings, LinkSettings, RatioController, Resampler, link};

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

#[test]
fn resampling_and_controlling_never_allocate() {
    let mut resampler = Resampler::with_defaults(2, 1024);
    let mut controller = RatioController::new(ControllerSettings::new(1200.0, 128, 48_000));
    let packet = vec![0.25f32; 480 * 2];
    let mut out = vec![0.0f32; 128 * 2];
    resampler.push(&packet);
    resampler.push(&packet);

    WATCHING.with(|w| w.set(true));
    let mut owed = 0.0;
    for _ in 0..10_000 {
        owed += 128.0 * 1.0003;
        while owed >= 480.0 {
            resampler.push(&packet);
            owed -= 480.0;
        }
        let ratio = controller.update(resampler.buffered() + owed);
        resampler.set_ratio(ratio);
        resampler.pull(&mut out);
    }
    resampler.reset();
    WATCHING.with(|w| w.set(false));
    assert_eq!(HEAP_CALLS.with(Cell::get), 0);
}

#[test]
fn a_link_never_allocates_after_it_is_made() {
    let (mut producer, mut consumer) = link(LinkSettings::new(2, 48_000, 480, 128));
    let packet = vec![0.25f32; 480 * 2];
    let mut out = vec![0.0f32; 128 * 2];

    WATCHING.with(|w| w.set(true));
    let (mut produced, mut now) = (0.0f64, 0u64);
    for _ in 0..10_000 {
        now += 128 * 1_000_000_000 / 48_000;
        produced += 128.0 * 1.0003;
        while produced >= 480.0 {
            producer.push(&packet, now);
            produced -= 480.0;
        }
        consumer.pull(&mut out, now);
    }
    let _ = consumer.stats();
    WATCHING.with(|w| w.set(false));
    assert_eq!(HEAP_CALLS.with(Cell::get), 0);
    assert!(consumer.stats().frames_out > 0);
}
