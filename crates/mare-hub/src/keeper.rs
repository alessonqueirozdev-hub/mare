//! The housekeeping thread: adopting clients, and letting them go.
//!
//! [`Housekeeper::poll`] makes one pass over the registry. It adopts each
//! client that asks (opening and checking its segment, opening its doorbell,
//! starting to watch its process) or refuses it with a reason; it takes back
//! the slots of clients that left or whose process ended; and it frees what
//! the cycle thread has handed back. It never waits for the cycle thread:
//! a slot it wants back is marked, and freed at a later pass once the cycle
//! thread has let go of it.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mare_protocol::registry::{RegistrySlot, reject_reason, slot_state};
use mare_protocol::ring::{RingConsumer, RingProducer};
use mare_protocol::segment::{SegmentHeader, SegmentLayout, SegmentView};
use mare_sys::{Doorbell, ProcessWatch, SharedMemory};

use crate::shared::{Attached, Counters, Shared, Stats, phase};

/// How long a refused client has to read why before its slot is freed
/// anyway.
const REJECTED_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    Left,
    Died,
}

#[derive(Default)]
struct Watch {
    process: Option<ProcessWatch>,
    ending: Option<Ending>,
    rejected_at: Option<Instant>,
}

/// The hub's housekeeping thread.
pub struct Housekeeper {
    shared: Arc<Shared>,
    watches: Vec<Watch>,
    waiter: mare_sys::Waiter,
}

impl Housekeeper {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        let watches = (0..shared.slots.len()).map(|_| Watch::default()).collect();
        Self {
            shared,
            watches,
            waiter: mare_sys::Waiter::coarse(),
        }
    }

    /// The hub's counters.
    pub fn stats(&self) -> Stats {
        self.shared.counters.snapshot()
    }

    /// Sleeps until a client rings the housekeeper's doorbell (it connects
    /// or leaves) or `timeout` passes. Returns whether it was rung.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        let bell = &self.shared.keeper_bell;
        let seen = bell.seen();
        bell.wait(&mut self.waiter, seen, timeout)
    }

    /// One pass over the registry.
    pub fn poll(&mut self) {
        for index in 0..self.shared.slots.len() {
            self.poll_slot(index);
        }
    }

    fn registry_slot(&self, index: usize) -> &RegistrySlot {
        self.shared
            .view
            .slot(index as u32)
            .expect("one hub slot per registry slot")
    }

    fn poll_slot(&mut self, index: usize) {
        let state = self.registry_slot(index).state.load(Ordering::Acquire);
        let current = self.shared.slots[index].phase.load(Ordering::Acquire);
        match current {
            phase::EMPTY => match state {
                slot_state::REQUESTED => self.adopt(index),
                slot_state::LEAVING => self.free(index),
                slot_state::REJECTED => {
                    let at = self.watches[index]
                        .rejected_at
                        .get_or_insert_with(Instant::now);
                    if at.elapsed() >= REJECTED_GRACE || self.process_ended(index) {
                        self.free(index);
                    }
                }
                slot_state::CLAIMED if self.process_ended(index) => self.free(index),
                _ => {}
            },
            phase::LIVE => {
                let ending = if state != slot_state::ACCEPTED {
                    // LEAVING, or a state only a confused client would store.
                    Some(Ending::Left)
                } else if self.watches[index]
                    .process
                    .as_ref()
                    .is_some_and(ProcessWatch::has_exited)
                {
                    Some(Ending::Died)
                } else {
                    None
                };
                if let Some(ending) = ending {
                    self.watches[index].ending = Some(ending);
                    self.shared.slots[index]
                        .phase
                        .store(phase::RETIRING, Ordering::Release);
                }
            }
            phase::RETIRED => {
                // SAFETY: this is the housekeeper and the phase is RETIRED.
                let attached = unsafe { self.shared.slots[index].keeper() }.take();
                drop(attached);
                let counter = match self.watches[index].ending {
                    Some(Ending::Died) => &self.shared.counters.died,
                    _ => &self.shared.counters.left,
                };
                Counters::add(counter, 1);
                self.free(index);
                self.shared.slots[index]
                    .phase
                    .store(phase::EMPTY, Ordering::Release);
            }
            _ => {}
        }
    }

    /// Whether the process that claimed slot `index` has ended (a process
    /// that cannot be watched counts as ended).
    fn process_ended(&self, index: usize) -> bool {
        match self.registry_slot(index).client_pid.load(Ordering::Acquire) {
            0 => false,
            pid => ProcessWatch::open(pid).map_or(true, |watch| watch.has_exited()),
        }
    }

    fn adopt(&mut self, index: usize) {
        match self.open_client(index) {
            Ok((attached, process)) => {
                // SAFETY: this is the housekeeper and the phase is EMPTY.
                *unsafe { self.shared.slots[index].keeper() } = Some(attached);
                self.watches[index] = Watch {
                    process: Some(process),
                    ..Watch::default()
                };
                self.shared.slots[index]
                    .phase
                    .store(phase::LIVE, Ordering::Release);
                let accepted = self
                    .registry_slot(index)
                    .state
                    .compare_exchange(
                        slot_state::REQUESTED,
                        slot_state::ACCEPTED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok();
                Counters::add(&self.shared.counters.accepted, 1);
                if !accepted {
                    // The client gave up while being adopted.
                    self.watches[index].ending = Some(Ending::Left);
                    self.shared.slots[index]
                        .phase
                        .store(phase::RETIRING, Ordering::Release);
                }
            }
            Err(reason) => {
                let slot = self.registry_slot(index);
                slot.reject_reason.store(reason, Ordering::Release);
                let _ = slot.state.compare_exchange(
                    slot_state::REQUESTED,
                    slot_state::REJECTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                self.watches[index] = Watch {
                    rejected_at: Some(Instant::now()),
                    ..Watch::default()
                };
                Counters::add(&self.shared.counters.rejected, 1);
            }
        }
    }

    fn open_client(&self, index: usize) -> Result<(Attached, ProcessWatch), u32> {
        let slot = self.registry_slot(index);
        let pid = slot.client_pid.load(Ordering::Acquire);
        let generation = slot.generation.load(Ordering::Acquire);
        let process = ProcessWatch::open(pid).map_err(|_| reject_reason::NO_PROCESS)?;
        let name = self.shared.name.segment(index as u32, generation);
        let segment = SharedMemory::open(&name).map_err(|_| reject_reason::NO_SEGMENT)?;
        // Nobody else needs to find it; this also keeps a client that dies
        // from leaving its name behind where names outlive their users.
        let _ = SharedMemory::unlink(&name);
        let mapped = segment.len() as u64;
        // SAFETY: `segment` maps `mapped` bytes.
        let header = unsafe { SegmentHeader::read_from(segment.base(), mapped) }
            .map_err(|_| reject_reason::BAD_SEGMENT)?;
        let layout =
            SegmentLayout::validate(&header, mapped).map_err(|_| reject_reason::BAD_SEGMENT)?;
        if layout.config().max_frames < self.shared.header.max_frames {
            return Err(reject_reason::FRAMES);
        }
        // SAFETY: validated against the mapped length; `Attached` keeps the
        // mapping as long as the view.
        let view = unsafe { SegmentView::from_raw(segment.base(), mapped, layout) }
            .map_err(|_| reject_reason::BAD_SEGMENT)?;
        // SAFETY: the bell is in the segment, kept with the doorbell.
        let go = unsafe {
            Doorbell::open(
                NonNull::from(&view.control().bell),
                &self.shared.name.client_wake(index as u32, generation),
            )
        }
        .map_err(|_| reject_reason::NO_DOORBELL)?;
        // SAFETY: the hub is the only producer of a client's input ring and
        // the only consumer of its output ring.
        let (events_in, events_out) = unsafe {
            (
                view.events_in().map(|r| RingProducer::new(r)),
                view.events_out().map(|r| RingConsumer::new(r)),
            )
        };
        Ok((
            Attached {
                events_in,
                events_out,
                go,
                view,
                generation,
                _segment: segment,
            },
            process,
        ))
    }

    /// Hands slot `index` back to the registry for a new client.
    fn free(&mut self, index: usize) {
        self.watches[index] = Watch::default();
        let slot = self.registry_slot(index);
        slot.client_pid.store(0, Ordering::Relaxed);
        slot.reject_reason
            .store(reject_reason::NONE, Ordering::Relaxed);
        slot.segment_size.store(0, Ordering::Relaxed);
        slot.cycles.store(0, Ordering::Relaxed);
        slot.late_cycles.store(0, Ordering::Relaxed);
        slot.generation.fetch_add(1, Ordering::Release);
        slot.state.store(slot_state::FREE, Ordering::Release);
    }
}
