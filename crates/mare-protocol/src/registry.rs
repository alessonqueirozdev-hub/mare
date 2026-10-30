//! The hub's registry: the channel clients connect through.
//!
//! A hub creates one registry segment under a name derived from its own (see
//! `mare-sys`). The registry holds, each block 64-byte aligned:
//!
//! | Block | Offset | Size |
//! |---|---|---|
//! | [`RegistryHeader`] | 0 | 64 bytes |
//! | [`RegistryControl`] | 64 | 128 bytes |
//! | `slot_count` × [`RegistrySlot`] | 192 | 64 bytes each |
//!
//! The hub writes the header, then stores [`hub_state::RUNNING`] with release
//! ordering. A client reads `hub_state` first, with acquire ordering, and
//! trusts the header only once it is `RUNNING`.
//!
//! # Connecting
//!
//! A slot's `state` moves through [`slot_state`]:
//!
//! 1. the client takes a `FREE` slot by compare-and-swap to `CLAIMED`, and
//!    reads its `generation`;
//! 2. it creates its segment under the name for (hub, slot, generation),
//!    stores its process id and the segment's size, and stores `REQUESTED`;
//! 3. the hub opens and validates the segment and stores `ACCEPTED`, or
//!    stores a [`reject_reason`] and `REJECTED`;
//! 4. a client that leaves stores `LEAVING`; the hub then stops using the
//!    segment and frees the slot. It also frees it, without being asked,
//!    when the client's process ends, or after a rejection.
//!
//! Freeing adds one to `generation` and stores `FREE`, so a new client in the
//! same slot never gets the names of the old one's objects.

use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64};

use crate::{BLOCK_ALIGN, LayoutError, VERSION_MAJOR, VERSION_MINOR};

/// The first eight bytes of a registry.
pub const REGISTRY_MAGIC: [u8; 8] = *b"MAREHUB\0";

/// The most client slots a registry may have.
pub const MAX_SLOTS: u32 = 1024;

/// Values of [`RegistryControl::hub_state`].
pub mod hub_state {
    /// The registry exists but its header is not written yet.
    pub const STARTING: u32 = 0;
    /// The hub is running and accepts clients.
    pub const RUNNING: u32 = 1;
    /// The hub has stopped; clients should leave.
    pub const STOPPED: u32 = 2;
}

/// Values of [`RegistrySlot::state`].
pub mod slot_state {
    /// Nobody uses the slot.
    pub const FREE: u32 = 0;
    /// A client has taken the slot and is preparing its segment.
    pub const CLAIMED: u32 = 1;
    /// The client's segment is ready for the hub to adopt.
    pub const REQUESTED: u32 = 2;
    /// The hub runs the client.
    pub const ACCEPTED: u32 = 3;
    /// The hub refused the client; `reject_reason` says why.
    pub const REJECTED: u32 = 4;
    /// The client is leaving.
    pub const LEAVING: u32 = 5;
}

/// Values of [`RegistrySlot::reject_reason`].
pub mod reject_reason {
    /// No reason given.
    pub const NONE: u32 = 0;
    /// The hub could not open the client's segment.
    pub const NO_SEGMENT: u32 = 1;
    /// The segment's header does not follow this layout.
    pub const BAD_SEGMENT: u32 = 2;
    /// The segment's buffers are shorter than the hub's longest period.
    pub const FRAMES: u32 = 3;
    /// The hub could not watch the client's process.
    pub const NO_PROCESS: u32 = 4;
    /// The hub could not open the client's doorbell.
    pub const NO_DOORBELL: u32 = 5;
    /// The hub runs as many clients as it can.
    pub const FULL: u32 = 6;
}

/// The plain fields of a registry, written once by the hub.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistryHeader {
    /// [`REGISTRY_MAGIC`].
    pub magic: [u8; 8],
    /// [`VERSION_MAJOR`] of the hub.
    pub version_major: u16,
    /// [`VERSION_MINOR`] of the hub.
    pub version_minor: u16,
    /// `size_of::<RegistryHeader>()`: 64.
    pub header_size: u32,
    /// Bytes in the whole registry.
    pub segment_size: u64,
    /// Offset of the [`RegistryControl`]: 64.
    pub control_offset: u64,
    /// Offset of the first slot: 192.
    pub slots_offset: u64,
    /// Number of slots.
    pub slot_count: u32,
    /// `size_of::<RegistrySlot>()`: 64.
    pub slot_size: u32,
    /// Frames per second of the master clock.
    pub sample_rate: u32,
    /// Frames in each cycle.
    pub period_frames: u32,
    /// The longest cycle the hub will run, in frames; a client's buffers must
    /// hold at least this many.
    pub max_frames: u32,
    /// The hub's process id.
    pub hub_pid: u32,
}

impl RegistryHeader {
    /// A copy of the header at the start of a mapping of `mapped` bytes,
    /// checked for alignment and length but not yet validated.
    ///
    /// # Safety
    ///
    /// `base` must point to `mapped` readable bytes.
    pub unsafe fn read_from(base: NonNull<u8>, mapped: u64) -> Result<Self, LayoutError> {
        if base.as_ptr() as usize % BLOCK_ALIGN != 0 {
            return Err(LayoutError::Misaligned);
        }
        if mapped < RegistryLayout::SLOTS_OFFSET {
            return Err(LayoutError::Truncated {
                needed: RegistryLayout::SLOTS_OFFSET,
                available: mapped,
            });
        }
        // SAFETY: aligned and long enough, as checked; any bytes are a valid
        // header.
        Ok(unsafe { base.cast::<Self>().read_volatile() })
    }
}

/// The hub-wide fields that change while it runs.
#[repr(C, align(64))]
pub struct RegistryControl {
    /// A [`hub_state`] value.
    pub hub_state: AtomicU32,
    _pad0: [u8; 4],
    /// The last cycle the hub started.
    pub cycle: AtomicU64,
    /// The housekeeper's doorbell: a client adds one when it asks to be
    /// adopted or leaves (layout 0.2; zero and unused before).
    pub keeper_bell: AtomicU32,
    _pad1: [u8; 44],
    /// The hub's doorbell: a client adds one when it finishes a cycle.
    pub hub_bell: AtomicU32,
    _pad2: [u8; 60],
}

/// One client's place in the registry.
#[repr(C, align(64))]
pub struct RegistrySlot {
    /// A [`slot_state`] value.
    pub state: AtomicU32,
    /// How many times the slot has been freed.
    pub generation: AtomicU32,
    /// The client's process id, stored before `REQUESTED`.
    pub client_pid: AtomicU32,
    /// A [`reject_reason`] value, stored before `REJECTED`.
    pub reject_reason: AtomicU32,
    /// The size of the client's segment, stored before `REQUESTED`.
    pub segment_size: AtomicU64,
    /// Cycles the client finished in time. Written by the hub.
    pub cycles: AtomicU64,
    /// Cycles the client missed. Written by the hub.
    pub late_cycles: AtomicU64,
    _reserved: [u8; 24],
}

/// Where everything is in a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistryLayout {
    slot_count: u32,
}

impl RegistryLayout {
    /// Offset of the [`RegistryControl`].
    pub const CONTROL_OFFSET: u64 = size_of::<RegistryHeader>() as u64;
    /// Offset of the first slot.
    pub const SLOTS_OFFSET: u64 = Self::CONTROL_OFFSET + size_of::<RegistryControl>() as u64;

    /// The layout of a registry with `slot_count` slots.
    pub const fn new(slot_count: u32) -> Result<Self, LayoutError> {
        if slot_count == 0 || slot_count > MAX_SLOTS {
            return Err(LayoutError::SlotCount);
        }
        Ok(Self { slot_count })
    }

    /// Number of slots.
    pub const fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// Bytes in the whole registry.
    pub const fn size(&self) -> u64 {
        Self::SLOTS_OFFSET + self.slot_count as u64 * size_of::<RegistrySlot>() as u64
    }

    /// The header a hub writes for this layout.
    pub const fn header(
        &self,
        sample_rate: u32,
        period_frames: u32,
        max_frames: u32,
        hub_pid: u32,
    ) -> RegistryHeader {
        RegistryHeader {
            magic: REGISTRY_MAGIC,
            version_major: VERSION_MAJOR,
            version_minor: VERSION_MINOR,
            header_size: size_of::<RegistryHeader>() as u32,
            segment_size: self.size(),
            control_offset: Self::CONTROL_OFFSET,
            slots_offset: Self::SLOTS_OFFSET,
            slot_count: self.slot_count,
            slot_size: size_of::<RegistrySlot>() as u32,
            sample_rate,
            period_frames,
            max_frames,
            hub_pid,
        }
    }

    /// Checks a header read from a registry of `mapped` bytes, and returns
    /// the layout it describes.
    pub fn validate(header: &RegistryHeader, mapped: u64) -> Result<Self, LayoutError> {
        if header.magic != REGISTRY_MAGIC {
            return Err(LayoutError::BadMagic);
        }
        if header.version_major != VERSION_MAJOR {
            return Err(LayoutError::VersionMismatch {
                found: header.version_major,
            });
        }
        let layout = Self::new(header.slot_count)?;
        let checks = [
            (
                header.header_size == size_of::<RegistryHeader>() as u32,
                "header_size",
            ),
            (header.segment_size == layout.size(), "segment_size"),
            (
                header.control_offset == Self::CONTROL_OFFSET,
                "control_offset",
            ),
            (header.slots_offset == Self::SLOTS_OFFSET, "slots_offset"),
            (
                header.slot_size == size_of::<RegistrySlot>() as u32,
                "slot_size",
            ),
            (header.period_frames > 0, "period_frames"),
            (header.period_frames <= header.max_frames, "max_frames"),
            (
                header.max_frames <= crate::segment::MAX_FRAMES,
                "max_frames",
            ),
            (header.sample_rate > 0, "sample_rate"),
        ];
        for (ok, field) in checks {
            if !ok {
                return Err(LayoutError::Mismatch { field });
            }
        }
        if layout.size() > mapped {
            return Err(LayoutError::Truncated {
                needed: layout.size(),
                available: mapped,
            });
        }
        Ok(layout)
    }
}

/// A mapped registry.
#[derive(Clone, Copy, Debug)]
pub struct RegistryView {
    base: NonNull<u8>,
    slot_count: u32,
}

// SAFETY: the view only hands out atomics and copies of plain data.
unsafe impl Send for RegistryView {}
// SAFETY: as above.
unsafe impl Sync for RegistryView {}

impl RegistryView {
    /// A view of the registry mapped at `base`, `mapped` bytes long, with
    /// the layout `layout`.
    ///
    /// # Safety
    ///
    /// `base` must point to `mapped` bytes that stay mapped, readable and
    /// writable while the view is used.
    pub unsafe fn from_raw(
        base: NonNull<u8>,
        mapped: u64,
        layout: RegistryLayout,
    ) -> Result<Self, LayoutError> {
        if base.as_ptr() as usize % BLOCK_ALIGN != 0 {
            return Err(LayoutError::Misaligned);
        }
        if layout.size() > mapped {
            return Err(LayoutError::Truncated {
                needed: layout.size(),
                available: mapped,
            });
        }
        Ok(Self {
            base,
            slot_count: layout.slot_count,
        })
    }

    /// The control block alone, which is all a client may read before
    /// `hub_state` is `RUNNING`.
    ///
    /// # Safety
    ///
    /// `base` must point to at least [`RegistryLayout::SLOTS_OFFSET`] mapped
    /// bytes, aligned to 64, that stay mapped while the reference lives.
    pub unsafe fn control_at<'a>(base: NonNull<u8>) -> &'a RegistryControl {
        // SAFETY: the caller guarantees the bytes; atomics accept any bytes.
        unsafe {
            base.add(RegistryLayout::CONTROL_OFFSET as usize)
                .cast::<RegistryControl>()
                .as_ref()
        }
    }

    /// Writes the header. Only the hub does this, before `RUNNING`.
    ///
    /// # Safety
    ///
    /// No other thread or process may be reading the header.
    pub unsafe fn write_header(&self, header: &RegistryHeader) {
        // SAFETY: the header is at offset zero, mapped and aligned.
        unsafe { self.base.cast::<RegistryHeader>().write_volatile(*header) };
    }

    /// A copy of the header as it is now.
    pub fn read_header(&self) -> RegistryHeader {
        // SAFETY: the header is at offset zero, mapped and aligned, and is
        // plain data.
        unsafe { self.base.cast::<RegistryHeader>().read_volatile() }
    }

    /// The control block.
    pub fn control(&self) -> &RegistryControl {
        // SAFETY: `from_raw` checked the whole layout is mapped.
        unsafe { Self::control_at(self.base) }
    }

    /// Number of slots.
    pub const fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// Slot `index`, if there is one.
    pub fn slot(&self, index: u32) -> Option<&RegistrySlot> {
        (index < self.slot_count).then(|| {
            let offset =
                RegistryLayout::SLOTS_OFFSET as usize + index as usize * size_of::<RegistrySlot>();
            // SAFETY: the slot is inside the mapped layout, aligned, and
            // made of atomics.
            unsafe { self.base.add(offset).cast::<RegistrySlot>().as_ref() }
        })
    }
}
