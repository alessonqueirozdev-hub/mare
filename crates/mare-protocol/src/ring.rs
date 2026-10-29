//! Rings of timestamped UMP events in shared memory.
//!
//! Each ring has exactly one producer and one consumer, in different
//! processes: the hub produces into a client's input ring and consumes its
//! output ring. A ring is a [`RingHeader`] followed by `capacity` entries of
//! [`UmpEvent`]. `head` and `tail` count events since the ring was created
//! and wrap around at 2³²; an entry's place is its count modulo the capacity,
//! which is a power of two.
//!
//! The other side is not trusted: a consumer that finds more events than the
//! ring can hold reports the ring as corrupt instead of reading out of
//! bounds, and a producer that finds it full refuses the event. Neither ever
//! waits.

use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

/// The largest capacity a ring may have, in events.
pub const MAX_RING_CAPACITY: u32 = 1 << 16;

/// One MIDI 2.0 Universal MIDI Packet, with the frame it belongs to.
///
/// `time` is in frames of the hub's master clock since the hub started,
/// the same clock as the audio (see [`ControlBlock::frame_time`]).
/// `words` holds the packet's 32-bit words in order; a packet shorter than
/// four words (see [`crate::ump::packet_words`]) leaves the rest zero.
///
/// [`ControlBlock::frame_time`]: crate::segment::ControlBlock::frame_time
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UmpEvent {
    /// The frame this event belongs to.
    pub time: u64,
    /// The packet's words.
    pub words: [u32; 4],
}

/// The two indices of a ring, on separate cache lines.
#[repr(C, align(64))]
pub struct RingHeader {
    /// Events written so far, modulo 2³². Written by the producer only.
    pub head: AtomicU32,
    _pad0: [u8; 60],
    /// Events read so far, modulo 2³². Written by the consumer only.
    pub tail: AtomicU32,
    _pad1: [u8; 60],
}

/// Bytes a ring of `capacity` events takes, header included.
pub const fn ring_bytes(capacity: u32) -> u64 {
    size_of::<RingHeader>() as u64 + capacity as u64 * size_of::<UmpEvent>() as u64
}

/// Whether `capacity` is allowed for a ring. Zero means no ring.
pub const fn valid_capacity(capacity: u32) -> bool {
    capacity == 0 || (capacity.is_power_of_two() && capacity <= MAX_RING_CAPACITY)
}

/// A consumer found more events in a ring than it can hold, which only a
/// misbehaving producer causes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingCorrupt;

/// A ring in a mapped segment.
#[derive(Clone, Copy, Debug)]
pub struct RingView {
    base: NonNull<u8>,
    capacity: u32,
}

// SAFETY: a ring lives in shared memory and is only reached through atomics
// and through the producer or consumer, whose constructors are unsafe and
// require a single owner of each role.
unsafe impl Send for RingView {}
// SAFETY: as above.
unsafe impl Sync for RingView {}

impl RingView {
    /// A view of the ring at `base`.
    ///
    /// # Safety
    ///
    /// `base` must point to [`ring_bytes`]`(capacity)` bytes of memory that
    /// stay mapped while the view and anything made from it is used, aligned
    /// to 64 bytes, and `capacity` must be a non-zero valid capacity.
    pub const unsafe fn from_raw(base: NonNull<u8>, capacity: u32) -> Self {
        Self { base, capacity }
    }

    /// The number of events the ring holds when full.
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The ring's indices.
    pub fn header(&self) -> &RingHeader {
        // SAFETY: `from_raw` requires a mapped, aligned header at `base`.
        unsafe { self.base.cast::<RingHeader>().as_ref() }
    }

    fn entry(&self, index: u32) -> *mut UmpEvent {
        let slot = (index & (self.capacity - 1)) as usize;
        // SAFETY: `slot < capacity`, and `from_raw` requires `capacity`
        // entries after the header.
        unsafe {
            self.base
                .as_ptr()
                .add(size_of::<RingHeader>())
                .cast::<UmpEvent>()
                .add(slot)
        }
    }

    /// Events waiting to be read, or [`RingCorrupt`] if the indices are
    /// further apart than the capacity.
    pub fn len(&self) -> Result<u32, RingCorrupt> {
        let header = self.header();
        let used = header
            .head
            .load(Ordering::Acquire)
            .wrapping_sub(header.tail.load(Ordering::Acquire));
        if used > self.capacity {
            Err(RingCorrupt)
        } else {
            Ok(used)
        }
    }

    /// Whether no events are waiting (a corrupt ring counts as not empty).
    pub fn is_empty(&self) -> bool {
        self.len() == Ok(0)
    }
}

/// The writing side of a ring.
#[derive(Debug)]
pub struct RingProducer {
    ring: RingView,
}

impl RingProducer {
    /// The producer of `ring`.
    ///
    /// # Safety
    ///
    /// No other producer of the same ring may exist at the same time, in this
    /// process or any other.
    pub const unsafe fn new(ring: RingView) -> Self {
        Self { ring }
    }

    /// Writes `event`, or hands it back if the ring is full (or the consumer
    /// has corrupted the indices).
    pub fn push(&mut self, event: UmpEvent) -> Result<(), UmpEvent> {
        let header = self.ring.header();
        let head = header.head.load(Ordering::Relaxed);
        let tail = header.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.ring.capacity {
            return Err(event);
        }
        // SAFETY: the entry is in bounds, and the consumer does not read it
        // until `head` is published below.
        unsafe { self.ring.entry(head).write_volatile(event) };
        header.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Room left, in events.
    pub fn free(&self) -> u32 {
        self.ring.capacity - self.ring.len().unwrap_or(self.ring.capacity)
    }

    /// The ring this produces into.
    pub const fn ring(&self) -> &RingView {
        &self.ring
    }
}

/// The reading side of a ring.
#[derive(Debug)]
pub struct RingConsumer {
    ring: RingView,
}

impl RingConsumer {
    /// The consumer of `ring`.
    ///
    /// # Safety
    ///
    /// No other consumer of the same ring may exist at the same time, in this
    /// process or any other.
    pub const unsafe fn new(ring: RingView) -> Self {
        Self { ring }
    }

    /// Reads the oldest event, if there is one.
    pub fn pop(&mut self) -> Result<Option<UmpEvent>, RingCorrupt> {
        let header = self.ring.header();
        let tail = header.tail.load(Ordering::Relaxed);
        let head = header.head.load(Ordering::Acquire);
        let used = head.wrapping_sub(tail);
        if used == 0 {
            return Ok(None);
        }
        if used > self.ring.capacity {
            return Err(RingCorrupt);
        }
        // SAFETY: the entry is in bounds, and the producer published it with
        // the release store of `head` read above.
        let event = unsafe { self.ring.entry(tail).read_volatile() };
        header.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(Some(event))
    }

    /// Drops every waiting event, which is how a consumer recovers from a
    /// corrupt ring.
    pub fn discard_all(&mut self) {
        let header = self.ring.header();
        let head = header.head.load(Ordering::Acquire);
        header.tail.store(head, Ordering::Release);
    }

    /// The ring this consumes from.
    pub const fn ring(&self) -> &RingView {
        &self.ring
    }
}
