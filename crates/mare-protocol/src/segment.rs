//! The segment a client shares with the hub.
//!
//! A client creates its segment, fills its [`SegmentHeader`], and asks the
//! hub to adopt it through a slot of the [registry](crate::registry). The
//! segment holds, in this order, each block starting on a 64-byte boundary:
//!
//! | Block | Size |
//! |---|---|
//! | [`SegmentHeader`] | 192 bytes |
//! | [`ControlBlock`] | 192 bytes |
//! | audio: input channels, then output channels | `channel_stride` bytes each |
//! | input event ring (hub to client), if any | [`ring_bytes`]`(events_in_capacity)` |
//! | output event ring (client to hub), if any | [`ring_bytes`]`(events_out_capacity)` |
//!
//! Every offset is a function of the five numbers of a [`SegmentConfig`],
//! computed by [`SegmentLayout::new`]. The header repeats the offsets so that
//! a reader in another language need not compute them, and a reader that
//! does (the hub) refuses a segment whose offsets differ from its own.
//!
//! An audio channel holds `max_frames` samples of 32-bit float, one period,
//! not interleaved. There is one buffer per channel, not two: the hub writes
//! a client's inputs only while the client is idle (it has finished the cycle
//! it was last woken for) and reads its outputs only after the client marks
//! the current cycle done, so the two sides never touch a buffer at once.

use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64};

use crate::ring::{RingView, ring_bytes, valid_capacity};
use crate::{BLOCK_ALIGN, LayoutError, VERSION_MAJOR, VERSION_MINOR, align_block};

/// The first eight bytes of a client segment.
pub const SEGMENT_MAGIC: [u8; 8] = *b"MARESEG\0";

/// The most audio channels a client may have in each direction.
pub const MAX_CHANNELS: u32 = 256;

/// The longest period, in frames, a segment can hold.
pub const MAX_FRAMES: u32 = 8192;

/// The longest client name, in bytes of UTF-8.
pub const NAME_CAPACITY: usize = 64;

/// The plain fields of a client segment, written once by the client before
/// it asks the hub to adopt the segment.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    /// [`SEGMENT_MAGIC`].
    pub magic: [u8; 8],
    /// [`VERSION_MAJOR`] of the writer.
    pub version_major: u16,
    /// [`VERSION_MINOR`] of the writer.
    pub version_minor: u16,
    /// `size_of::<SegmentHeader>()`: 192.
    pub header_size: u32,
    /// Bytes in the whole segment.
    pub segment_size: u64,
    /// Offset of the [`ControlBlock`].
    pub control_offset: u64,
    /// Offset of the first audio channel.
    pub audio_offset: u64,
    /// Bytes from one audio channel to the next.
    pub channel_stride: u32,
    /// Audio channels the hub writes (the client's inputs).
    pub input_channels: u32,
    /// Audio channels the client writes (its outputs).
    pub output_channels: u32,
    /// The longest period the audio channels can hold.
    pub max_frames: u32,
    /// Offset of the input event ring, or zero if there is none.
    pub events_in_offset: u64,
    /// Offset of the output event ring, or zero if there is none.
    pub events_out_offset: u64,
    /// Capacity of the input event ring, in events.
    pub events_in_capacity: u32,
    /// Capacity of the output event ring, in events.
    pub events_out_capacity: u32,
    /// Latency the client adds, in frames, as it declares it.
    pub latency_frames: u32,
    /// Bytes of `name` in use.
    pub name_len: u32,
    /// The client's name, UTF-8, for people to read.
    pub name: [u8; NAME_CAPACITY],
    /// Zero. Later minor versions may give these bytes a meaning.
    pub reserved: [u8; 40],
}

/// The fields of a client segment that change while it runs.
///
/// The first cache line is written by the hub, the second by the client, and
/// the third holds the client's doorbell, which the hub rings to wake it.
#[repr(C, align(64))]
pub struct ControlBlock {
    /// The cycle the client is woken for, counted from 1; zero before the
    /// first. Written by the hub (release) after the inputs are in place.
    pub cycle: AtomicU64,
    /// The frame of the master clock at which this cycle's first frame is.
    pub frame_time: AtomicU64,
    /// Frames in this cycle, at most `max_frames`.
    pub frames: AtomicU32,
    /// [`hub_flags`] bits.
    pub hub_flags: AtomicU32,
    /// How long, in nanoseconds from being woken, the client has before the
    /// hub stops waiting for it this cycle.
    pub budget_ns: AtomicU64,
    _pad0: [u8; 32],
    /// The last cycle the client finished. Written by the client (release)
    /// after its outputs are in place.
    pub done_cycle: AtomicU64,
    /// [`client_flags`] bits.
    pub client_flags: AtomicU32,
    _pad1: [u8; 52],
    /// The client's doorbell: the hub adds one to wake the client.
    pub bell: AtomicU32,
    _pad2: [u8; 60],
}

/// Bits of [`ControlBlock::hub_flags`].
pub mod hub_flags {
    /// The hub asks the client to stop: it is shutting down or has dropped
    /// the client.
    pub const STOP: u32 = 1 << 0;
}

/// Bits of [`ControlBlock::client_flags`].
pub mod client_flags {
    /// The client is leaving and will not finish another cycle.
    pub const LEAVING: u32 = 1 << 0;
}

/// The five numbers a client segment's layout follows from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentConfig {
    /// Audio channels into the client.
    pub input_channels: u32,
    /// Audio channels out of the client.
    pub output_channels: u32,
    /// The longest period the buffers hold, in frames.
    pub max_frames: u32,
    /// Capacity of the ring of events into the client (zero or a power of
    /// two).
    pub events_in_capacity: u32,
    /// Capacity of the ring of events out of the client (zero or a power of
    /// two).
    pub events_out_capacity: u32,
}

/// Where everything is in a client segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentLayout {
    config: SegmentConfig,
    control_offset: u64,
    audio_offset: u64,
    channel_stride: u64,
    events_in_offset: u64,
    events_out_offset: u64,
    size: u64,
}

impl SegmentLayout {
    /// The layout for `config`, or why there can be none.
    pub const fn new(config: SegmentConfig) -> Result<Self, LayoutError> {
        if config.input_channels > MAX_CHANNELS || config.output_channels > MAX_CHANNELS {
            return Err(LayoutError::TooManyChannels);
        }
        if config.max_frames == 0 || config.max_frames > MAX_FRAMES {
            return Err(LayoutError::FramesOutOfRange);
        }
        if !valid_capacity(config.events_in_capacity) || !valid_capacity(config.events_out_capacity)
        {
            return Err(LayoutError::RingCapacity);
        }
        // With the limits above nothing below can overflow a u64, so the
        // arithmetic is plain; `align_block` cannot fail either.
        let control_offset = size_of::<SegmentHeader>() as u64;
        let audio_offset = control_offset + size_of::<ControlBlock>() as u64;
        let channel_stride = match align_block(config.max_frames as u64 * 4) {
            Some(v) => v,
            None => return Err(LayoutError::FramesOutOfRange),
        };
        let channels = config.input_channels as u64 + config.output_channels as u64;
        let mut end = audio_offset + channels * channel_stride;
        let mut events_in_offset = 0;
        if config.events_in_capacity > 0 {
            events_in_offset = end;
            end = match align_block(end + ring_bytes(config.events_in_capacity)) {
                Some(v) => v,
                None => return Err(LayoutError::RingCapacity),
            };
        }
        let mut events_out_offset = 0;
        if config.events_out_capacity > 0 {
            events_out_offset = end;
            end = match align_block(end + ring_bytes(config.events_out_capacity)) {
                Some(v) => v,
                None => return Err(LayoutError::RingCapacity),
            };
        }
        Ok(Self {
            config,
            control_offset,
            audio_offset,
            channel_stride,
            events_in_offset,
            events_out_offset,
            size: end,
        })
    }

    /// The configuration this layout follows from.
    pub const fn config(&self) -> SegmentConfig {
        self.config
    }

    /// Bytes in the whole segment.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Offset of the [`ControlBlock`].
    pub const fn control_offset(&self) -> u64 {
        self.control_offset
    }

    /// Offset of the first audio channel.
    pub const fn audio_offset(&self) -> u64 {
        self.audio_offset
    }

    /// Bytes from one audio channel to the next.
    pub const fn channel_stride(&self) -> u64 {
        self.channel_stride
    }

    /// Offset of the input event ring, or zero.
    pub const fn events_in_offset(&self) -> u64 {
        self.events_in_offset
    }

    /// Offset of the output event ring, or zero.
    pub const fn events_out_offset(&self) -> u64 {
        self.events_out_offset
    }

    /// The header a client writes for this layout.
    pub fn header(&self, name: &str, latency_frames: u32) -> Result<SegmentHeader, LayoutError> {
        if name.len() > NAME_CAPACITY {
            return Err(LayoutError::NameTooLong);
        }
        let mut name_bytes = [0; NAME_CAPACITY];
        name_bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(SegmentHeader {
            magic: SEGMENT_MAGIC,
            version_major: VERSION_MAJOR,
            version_minor: VERSION_MINOR,
            header_size: size_of::<SegmentHeader>() as u32,
            segment_size: self.size,
            control_offset: self.control_offset,
            audio_offset: self.audio_offset,
            // At most MAX_FRAMES * 4 rounded up: fits.
            channel_stride: self.channel_stride as u32,
            input_channels: self.config.input_channels,
            output_channels: self.config.output_channels,
            max_frames: self.config.max_frames,
            events_in_offset: self.events_in_offset,
            events_out_offset: self.events_out_offset,
            events_in_capacity: self.config.events_in_capacity,
            events_out_capacity: self.config.events_out_capacity,
            latency_frames,
            name_len: name.len() as u32,
            name: name_bytes,
            reserved: [0; 40],
        })
    }

    /// Checks a header read from a segment of `mapped` bytes, and returns the
    /// layout it describes.
    ///
    /// Every offset must be the one [`SegmentLayout::new`] computes for the
    /// header's configuration, so a header that passes cannot point outside
    /// its segment or make two blocks overlap.
    pub fn validate(header: &SegmentHeader, mapped: u64) -> Result<Self, LayoutError> {
        if header.magic != SEGMENT_MAGIC {
            return Err(LayoutError::BadMagic);
        }
        if header.version_major != VERSION_MAJOR {
            return Err(LayoutError::VersionMismatch {
                found: header.version_major,
            });
        }
        if header.header_size != size_of::<SegmentHeader>() as u32 {
            return Err(LayoutError::Mismatch {
                field: "header_size",
            });
        }
        let layout = Self::new(SegmentConfig {
            input_channels: header.input_channels,
            output_channels: header.output_channels,
            max_frames: header.max_frames,
            events_in_capacity: header.events_in_capacity,
            events_out_capacity: header.events_out_capacity,
        })?;
        let checks = [
            (header.segment_size == layout.size, "segment_size"),
            (
                header.control_offset == layout.control_offset,
                "control_offset",
            ),
            (header.audio_offset == layout.audio_offset, "audio_offset"),
            (
                header.channel_stride as u64 == layout.channel_stride,
                "channel_stride",
            ),
            (
                header.events_in_offset == layout.events_in_offset,
                "events_in_offset",
            ),
            (
                header.events_out_offset == layout.events_out_offset,
                "events_out_offset",
            ),
            (header.name_len as usize <= NAME_CAPACITY, "name_len"),
        ];
        for (ok, field) in checks {
            if !ok {
                return Err(LayoutError::Mismatch { field });
            }
        }
        if layout.size > mapped {
            return Err(LayoutError::Truncated {
                needed: layout.size,
                available: mapped,
            });
        }
        Ok(layout)
    }
}

impl SegmentHeader {
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
        let needed = size_of::<Self>() as u64;
        if mapped < needed {
            return Err(LayoutError::Truncated {
                needed,
                available: mapped,
            });
        }
        // SAFETY: aligned and long enough, as checked; any bytes are a valid
        // header.
        Ok(unsafe { base.cast::<Self>().read_volatile() })
    }

    /// The client's name, or `None` if its bytes are not UTF-8.
    pub fn name(&self) -> Option<&str> {
        let len = (self.name_len as usize).min(NAME_CAPACITY);
        core::str::from_utf8(&self.name[..len]).ok()
    }
}

/// A mapped client segment, seen through its layout.
#[derive(Clone, Copy, Debug)]
pub struct SegmentView {
    base: NonNull<u8>,
    layout: SegmentLayout,
}

// SAFETY: the view only hands out atomics, ring views and raw pointers; using
// the raw audio pointers is unsafe and governed by the cycle protocol.
unsafe impl Send for SegmentView {}
// SAFETY: as above.
unsafe impl Sync for SegmentView {}

impl SegmentView {
    /// A view of the segment mapped at `base`, `mapped` bytes long.
    ///
    /// # Safety
    ///
    /// `base` must point to `mapped` bytes that stay mapped, readable and
    /// writable while the view and anything made from it is used.
    pub unsafe fn from_raw(
        base: NonNull<u8>,
        mapped: u64,
        layout: SegmentLayout,
    ) -> Result<Self, LayoutError> {
        if base.as_ptr() as usize % BLOCK_ALIGN != 0 {
            return Err(LayoutError::Misaligned);
        }
        if layout.size > mapped {
            return Err(LayoutError::Truncated {
                needed: layout.size,
                available: mapped,
            });
        }
        Ok(Self { base, layout })
    }

    /// The layout this view follows.
    pub const fn layout(&self) -> &SegmentLayout {
        &self.layout
    }

    fn at(&self, offset: u64) -> NonNull<u8> {
        // SAFETY: every offset passed here is inside the layout, which
        // `from_raw` checked is mapped.
        unsafe { self.base.add(offset as usize) }
    }

    /// Writes `header` at the start of the segment. Only its creator does
    /// this, before anyone else can see the segment.
    ///
    /// # Safety
    ///
    /// No other thread or process may be reading the header.
    pub unsafe fn write_header(&self, header: &SegmentHeader) {
        // SAFETY: the header is at offset zero, mapped and aligned; the
        // caller guarantees nobody reads it concurrently.
        unsafe { self.base.cast::<SegmentHeader>().write_volatile(*header) };
    }

    /// A copy of the header as it is now. A reader validates this copy with
    /// [`SegmentLayout::validate`] and never reads the header again.
    pub fn read_header(&self) -> SegmentHeader {
        // SAFETY: the header is at offset zero, mapped and aligned. It is
        // plain data, so any bytes are a valid value.
        unsafe { self.base.cast::<SegmentHeader>().read_volatile() }
    }

    /// The control block.
    pub fn control(&self) -> &ControlBlock {
        // SAFETY: the control block is mapped and aligned, and made of
        // atomics, which any bytes are valid values of.
        unsafe {
            self.at(self.layout.control_offset)
                .cast::<ControlBlock>()
                .as_ref()
        }
    }

    /// The first sample of input channel `channel`, which has `max_frames`
    /// samples. `None` if there is no such channel.
    pub fn input(&self, channel: u32) -> Option<NonNull<f32>> {
        (channel < self.layout.config.input_channels).then(|| {
            self.at(self.layout.audio_offset + channel as u64 * self.layout.channel_stride)
                .cast()
        })
    }

    /// The first sample of output channel `channel`, which has `max_frames`
    /// samples. `None` if there is no such channel.
    pub fn output(&self, channel: u32) -> Option<NonNull<f32>> {
        let index = self.layout.config.input_channels as u64 + channel as u64;
        (channel < self.layout.config.output_channels).then(|| {
            self.at(self.layout.audio_offset + index * self.layout.channel_stride)
                .cast()
        })
    }

    /// The ring of events into the client, if it has one.
    pub fn events_in(&self) -> Option<RingView> {
        let capacity = self.layout.config.events_in_capacity;
        (capacity > 0).then(|| {
            // SAFETY: the layout puts a ring of this capacity at this
            // offset, inside the mapping, 64-byte aligned.
            unsafe { RingView::from_raw(self.at(self.layout.events_in_offset), capacity) }
        })
    }

    /// The ring of events out of the client, if it has one.
    pub fn events_out(&self) -> Option<RingView> {
        let capacity = self.layout.config.events_out_capacity;
        (capacity > 0).then(|| {
            // SAFETY: as in `events_in`.
            unsafe { RingView::from_raw(self.at(self.layout.events_out_offset), capacity) }
        })
    }
}
