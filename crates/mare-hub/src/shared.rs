//! What the cycle thread and the housekeeping thread share.
//!
//! The hub keeps one [`HubSlot`] per registry slot. A slot's `phase` says
//! which thread owns the client's mapping and handles, so that neither ever
//! waits for the other:
//!
//! | Phase | Owner | Next |
//! |---|---|---|
//! | `EMPTY` | housekeeper | `LIVE`, once it has adopted a client |
//! | `LIVE` | cycle thread | `RETIRING`, when the housekeeper wants it back |
//! | `RETIRING` | cycle thread | `RETIRED`, stored by the cycle thread at its next cycle |
//! | `RETIRED` | housekeeper | `EMPTY`, once it has dropped the mapping |
//!
//! Only the housekeeper creates or drops a client's resources, so the cycle
//! thread never allocates or frees.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use mare_protocol::registry::{LatencySource, RegistryHeader, RegistryView, hub_state};
use mare_protocol::ring::{RingConsumer, RingProducer};
use mare_protocol::segment::{SegmentView, hub_flags};
use mare_sys::{Doorbell, HubName, SharedMemory};

use crate::device::DeviceLatency;

pub(crate) mod phase {
    pub(crate) const EMPTY: u32 = 0;
    pub(crate) const LIVE: u32 = 1;
    pub(crate) const RETIRING: u32 = 2;
    pub(crate) const RETIRED: u32 = 3;
}

/// A client's resources, while the hub has adopted it.
pub(crate) struct Attached {
    // Drop order: the rings, the doorbell and the view point into the
    // segment, which goes last.
    pub(crate) events_in: Option<RingProducer>,
    pub(crate) events_out: Option<RingConsumer>,
    pub(crate) go: Doorbell,
    pub(crate) view: SegmentView,
    pub(crate) generation: u32,
    pub(crate) _segment: SharedMemory,
}

pub(crate) struct HubSlot {
    pub(crate) phase: AtomicU32,
    attached: UnsafeCell<Option<Attached>>,
}

// SAFETY: `attached` is reached only by the thread that owns the slot's
// current phase, and phases are handed over with release and acquire.
unsafe impl Sync for HubSlot {}

impl HubSlot {
    pub(crate) fn new() -> Self {
        Self {
            phase: AtomicU32::new(phase::EMPTY),
            attached: UnsafeCell::new(None),
        }
    }

    /// The resources, for the housekeeper.
    ///
    /// # Safety
    ///
    /// Only the housekeeper may call this, while the phase is `EMPTY` or
    /// `RETIRED` (loaded with acquire ordering).
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn keeper(&self) -> &mut Option<Attached> {
        // SAFETY: the caller owns the slot in this phase.
        unsafe { &mut *self.attached.get() }
    }

    /// The resources, for the cycle thread.
    ///
    /// # Safety
    ///
    /// Only the cycle thread may call this, after loading `LIVE` or
    /// `RETIRING` with acquire ordering and before storing `RETIRED`.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn engine(&self) -> Option<&mut Attached> {
        // SAFETY: the caller owns the slot in this phase.
        unsafe { (*self.attached.get()).as_mut() }
    }
}

/// Counters of what the hub has done, updated as it runs.
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) cycles: AtomicU64,
    pub(crate) client_cycles: AtomicU64,
    pub(crate) late: AtomicU64,
    pub(crate) busy: AtomicU64,
    pub(crate) accepted: AtomicU64,
    pub(crate) rejected: AtomicU64,
    pub(crate) left: AtomicU64,
    pub(crate) died: AtomicU64,
    pub(crate) events_received: AtomicU64,
    pub(crate) events_corrupt: AtomicU64,
    pub(crate) longest_wait_ns: AtomicU64,
}

/// A copy of the hub's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Cycles the hub has run.
    pub cycles: u64,
    /// Client cycles finished in time, over all clients.
    pub client_cycles: u64,
    /// Client cycles a client was woken for and did not finish in time.
    pub late: u64,
    /// Client cycles a client was not woken for, because it was still
    /// working on an earlier one.
    pub busy: u64,
    /// Clients adopted.
    pub accepted: u64,
    /// Clients refused.
    pub rejected: u64,
    /// Clients that left.
    pub left: u64,
    /// Clients whose process ended while adopted.
    pub died: u64,
    /// Events read from clients' output rings.
    pub events_received: u64,
    /// Times a client's output ring was found corrupt and emptied.
    pub events_corrupt: u64,
    /// The longest time, in nanoseconds, the hub has waited for its clients
    /// in one cycle.
    pub longest_wait_ns: u64,
}

impl Counters {
    pub(crate) fn snapshot(&self) -> Stats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Stats {
            cycles: get(&self.cycles),
            client_cycles: get(&self.client_cycles),
            late: get(&self.late),
            busy: get(&self.busy),
            accepted: get(&self.accepted),
            rejected: get(&self.rejected),
            left: get(&self.left),
            died: get(&self.died),
            events_received: get(&self.events_received),
            events_corrupt: get(&self.events_corrupt),
            longest_wait_ns: get(&self.longest_wait_ns),
        }
    }

    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

pub(crate) struct Shared {
    // Drop order: everything that points into the registry goes before it.
    pub(crate) slots: Box<[HubSlot]>,
    pub(crate) hub_bell: Doorbell,
    pub(crate) keeper_bell: Doorbell,
    pub(crate) view: RegistryView,
    pub(crate) header: RegistryHeader,
    pub(crate) name: HubName,
    pub(crate) counters: Counters,
    pub(crate) _registry: SharedMemory,
}

impl Shared {
    /// Publishes the device's latency, from `source`, to clients.
    pub(crate) fn publish_latency(&self, latency: DeviceLatency, source: LatencySource) {
        let control = self.view.control();
        let source = if latency.is_unknown() {
            LatencySource::Unknown
        } else {
            source
        };
        control
            .input_latency
            .store(latency.input_frames, Ordering::Relaxed);
        control
            .output_latency
            .store(latency.output_frames, Ordering::Relaxed);
        control
            .latency_source
            .store(source.raw(), Ordering::Release);
    }

    /// The latency published, and where it comes from.
    pub(crate) fn latency(&self) -> (DeviceLatency, LatencySource) {
        let control = self.view.control();
        let source = LatencySource::from_raw(control.latency_source.load(Ordering::Acquire));
        let latency = DeviceLatency {
            input_frames: control.input_latency.load(Ordering::Relaxed),
            output_frames: control.output_latency.load(Ordering::Relaxed),
        };
        (latency, source)
    }

    /// Tells clients the hub is stopping: the registry says so, and every
    /// client the cycle thread still runs is asked to stop and woken.
    ///
    /// Called by the cycle thread when it stops, which owns the `LIVE`
    /// slots.
    pub(crate) fn shut_down(&self) {
        self.view
            .control()
            .hub_state
            .store(hub_state::STOPPED, Ordering::Release);
        for slot in self.slots.iter() {
            let phase = slot.phase.load(Ordering::Acquire);
            if phase == phase::LIVE || phase == phase::RETIRING {
                // SAFETY: called from the cycle thread, which owns these
                // phases.
                if let Some(client) = unsafe { slot.engine() } {
                    client
                        .view
                        .control()
                        .hub_flags
                        .fetch_or(hub_flags::STOP, Ordering::Release);
                    client.go.ring();
                }
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.view
            .control()
            .hub_state
            .store(hub_state::STOPPED, Ordering::Release);
    }
}
