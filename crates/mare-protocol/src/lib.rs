//! The shared-memory layout of MARÉ.
//!
//! MARÉ moves audio and MIDI between a hub and its clients through shared
//! memory. This crate is that memory's specification in code: the structures,
//! their offsets, and the rules for reading and writing them. It has no
//! dependencies and does not use the standard library, so the same layout can
//! be checked from a C header or another language against the same numbers.
//!
//! There are two kinds of segment:
//!
//! - the **registry** ([`registry`]), one per hub, created by the hub under a
//!   name derived from the hub's name. It says how the hub runs (sample rate,
//!   period) and holds one slot per client that can connect;
//! - the **client segment** ([`segment`]), one per client, created by the
//!   client. It holds the client's [`ControlBlock`](segment::ControlBlock),
//!   its audio buffers, and its two rings of timestamped UMP events
//!   ([`ring`]).
//!
//! Rules every structure here follows:
//!
//! - no pointers: positions are byte offsets from the start of the segment;
//! - C layout (`#[repr(C)]`), explicit sizes, every block 64-byte aligned so
//!   that fields written by different sides never share a cache line;
//! - little-endian only (a big-endian build does not compile);
//! - fields written by one side and read by the other are atomics; plain
//!   fields are written once, before the segment is published, and a reader
//!   copies and validates them once instead of trusting them later;
//! - a major version that must match, and a minor version that may differ.
//!
//! Nothing here allocates, locks or makes a system call.

#![no_std]

#[cfg(not(target_endian = "little"))]
compile_error!("MARÉ's shared memory is little-endian; big-endian hosts are not supported");

pub mod registry;
pub mod ring;
pub mod segment;
pub mod ump;

use core::fmt;

/// The major version of the layout. A reader refuses any other.
pub const VERSION_MAJOR: u16 = 0;

/// The minor version of the layout. Minor versions only add meaning to
/// reserved bytes, so a reader accepts any minor version.
pub const VERSION_MINOR: u16 = 3;

/// Every block in a segment starts on a multiple of this many bytes, and the
/// segment itself must be mapped at such an address.
pub const BLOCK_ALIGN: usize = 64;

/// Why a layout could not be built or a segment was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutError {
    /// More channels than [`segment::MAX_CHANNELS`].
    TooManyChannels,
    /// A period of zero frames, or more than [`segment::MAX_FRAMES`].
    FramesOutOfRange,
    /// A ring capacity that is not zero or a power of two up to
    /// [`ring::MAX_RING_CAPACITY`].
    RingCapacity,
    /// No slots, or more than [`registry::MAX_SLOTS`].
    SlotCount,
    /// A client name longer than [`segment::NAME_CAPACITY`] bytes.
    NameTooLong,
    /// The magic bytes at the start are not this kind of segment's.
    BadMagic,
    /// The segment's major version is not [`VERSION_MAJOR`].
    VersionMismatch {
        /// The major version the segment says it has.
        found: u16,
    },
    /// A field does not hold the value this layout requires.
    Mismatch {
        /// The field's name, as in the structure.
        field: &'static str,
    },
    /// The mapping is shorter than the layout needs.
    Truncated {
        /// Bytes the layout needs.
        needed: u64,
        /// Bytes that are mapped.
        available: u64,
    },
    /// The mapping does not start on a [`BLOCK_ALIGN`] boundary.
    Misaligned,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyChannels => write!(f, "too many channels"),
            Self::FramesOutOfRange => write!(f, "period length out of range"),
            Self::RingCapacity => write!(f, "ring capacity must be zero or a power of two"),
            Self::SlotCount => write!(f, "slot count out of range"),
            Self::NameTooLong => write!(f, "client name too long"),
            Self::BadMagic => write!(f, "not a MARÉ segment"),
            Self::VersionMismatch { found } => {
                write!(f, "layout version {found}, expected {VERSION_MAJOR}")
            }
            Self::Mismatch { field } => write!(f, "field `{field}` does not match the layout"),
            Self::Truncated { needed, available } => {
                write!(f, "segment needs {needed} bytes, {available} mapped")
            }
            Self::Misaligned => write!(f, "segment is not 64-byte aligned"),
        }
    }
}

impl core::error::Error for LayoutError {}

/// Rounds `value` up to a multiple of [`BLOCK_ALIGN`], or `None` on overflow.
pub(crate) const fn align_block(value: u64) -> Option<u64> {
    let mask = BLOCK_ALIGN as u64 - 1;
    match value.checked_add(mask) {
        Some(v) => Some(v & !mask),
        None => None,
    }
}
