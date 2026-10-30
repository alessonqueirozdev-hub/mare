//! The layout, checked from both sides.
//!
//! One side is the Rust structures. The other is a reader that knows only
//! the byte offsets written in the specification (the tables in
//! `docs/LAYOUT.md`) and decodes little-endian integers from raw bytes, as a
//! program in another language would. A segment written through the
//! structures must read correctly through the offsets, and the other way
//! round.

use std::mem::{align_of, offset_of, size_of};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use mare_protocol::LayoutError;
use mare_protocol::registry::{
    RegistryControl, RegistryHeader, RegistryLayout, RegistrySlot, RegistryView, hub_state,
    slot_state,
};
use mare_protocol::ring::{RingHeader, UmpEvent};
use mare_protocol::segment::{
    ControlBlock, SegmentConfig, SegmentHeader, SegmentLayout, SegmentView,
};

/// 64 bytes, 64-byte aligned, like a block of a mapping.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Block([u8; 64]);

/// Zeroed memory aligned to 64 bytes, standing in for a mapping.
struct Mapping {
    blocks: Vec<Block>,
}

impl Mapping {
    fn new(bytes: u64) -> Self {
        Self {
            blocks: vec![Block([0; 64]); bytes.div_ceil(64) as usize],
        }
    }

    fn base(&mut self) -> NonNull<u8> {
        NonNull::new(self.blocks.as_mut_ptr().cast()).unwrap()
    }

    fn len(&self) -> u64 {
        self.blocks.len() as u64 * 64
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the blocks are plain integers, viewed as bytes.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast(), self.len() as usize) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self.len() as usize;
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts_mut(self.blocks.as_mut_ptr().cast(), len) }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

const CONFIG: SegmentConfig = SegmentConfig {
    input_channels: 2,
    output_channels: 3,
    max_frames: 100,
    events_in_capacity: 64,
    events_out_capacity: 16,
};

#[test]
fn sizes_and_alignments_are_the_specified_ones() {
    assert_eq!(size_of::<SegmentHeader>(), 192);
    assert_eq!(size_of::<ControlBlock>(), 192);
    assert_eq!(size_of::<RingHeader>(), 128);
    assert_eq!(size_of::<UmpEvent>(), 24);
    assert_eq!(size_of::<RegistryHeader>(), 64);
    assert_eq!(size_of::<RegistryControl>(), 128);
    assert_eq!(size_of::<RegistrySlot>(), 64);
    for align in [
        align_of::<SegmentHeader>(),
        align_of::<ControlBlock>(),
        align_of::<RingHeader>(),
        align_of::<RegistryHeader>(),
        align_of::<RegistryControl>(),
        align_of::<RegistrySlot>(),
    ] {
        assert_eq!(align, 64);
    }
    assert_eq!(align_of::<UmpEvent>(), 8);
}

#[test]
fn field_offsets_are_the_specified_ones() {
    let segment_header = [
        (offset_of!(SegmentHeader, magic), 0),
        (offset_of!(SegmentHeader, version_major), 8),
        (offset_of!(SegmentHeader, version_minor), 10),
        (offset_of!(SegmentHeader, header_size), 12),
        (offset_of!(SegmentHeader, segment_size), 16),
        (offset_of!(SegmentHeader, control_offset), 24),
        (offset_of!(SegmentHeader, audio_offset), 32),
        (offset_of!(SegmentHeader, channel_stride), 40),
        (offset_of!(SegmentHeader, input_channels), 44),
        (offset_of!(SegmentHeader, output_channels), 48),
        (offset_of!(SegmentHeader, max_frames), 52),
        (offset_of!(SegmentHeader, events_in_offset), 56),
        (offset_of!(SegmentHeader, events_out_offset), 64),
        (offset_of!(SegmentHeader, events_in_capacity), 72),
        (offset_of!(SegmentHeader, events_out_capacity), 76),
        (offset_of!(SegmentHeader, latency_frames), 80),
        (offset_of!(SegmentHeader, name_len), 84),
        (offset_of!(SegmentHeader, name), 88),
        (offset_of!(SegmentHeader, reserved), 152),
    ];
    let control = [
        (offset_of!(ControlBlock, cycle), 0),
        (offset_of!(ControlBlock, frame_time), 8),
        (offset_of!(ControlBlock, frames), 16),
        (offset_of!(ControlBlock, hub_flags), 20),
        (offset_of!(ControlBlock, budget_ns), 24),
        (offset_of!(ControlBlock, done_cycle), 64),
        (offset_of!(ControlBlock, client_flags), 72),
        (offset_of!(ControlBlock, bell), 128),
    ];
    let ring = [
        (offset_of!(RingHeader, head), 0),
        (offset_of!(RingHeader, tail), 64),
        (offset_of!(UmpEvent, time), 0),
        (offset_of!(UmpEvent, words), 8),
    ];
    let registry = [
        (offset_of!(RegistryHeader, magic), 0),
        (offset_of!(RegistryHeader, version_major), 8),
        (offset_of!(RegistryHeader, version_minor), 10),
        (offset_of!(RegistryHeader, header_size), 12),
        (offset_of!(RegistryHeader, segment_size), 16),
        (offset_of!(RegistryHeader, control_offset), 24),
        (offset_of!(RegistryHeader, slots_offset), 32),
        (offset_of!(RegistryHeader, slot_count), 40),
        (offset_of!(RegistryHeader, slot_size), 44),
        (offset_of!(RegistryHeader, sample_rate), 48),
        (offset_of!(RegistryHeader, period_frames), 52),
        (offset_of!(RegistryHeader, max_frames), 56),
        (offset_of!(RegistryHeader, hub_pid), 60),
        (offset_of!(RegistryControl, hub_state), 0),
        (offset_of!(RegistryControl, cycle), 8),
        (offset_of!(RegistryControl, keeper_bell), 16),
        (offset_of!(RegistryControl, hub_bell), 64),
        (offset_of!(RegistrySlot, state), 0),
        (offset_of!(RegistrySlot, generation), 4),
        (offset_of!(RegistrySlot, client_pid), 8),
        (offset_of!(RegistrySlot, reject_reason), 12),
        (offset_of!(RegistrySlot, segment_size), 16),
        (offset_of!(RegistrySlot, cycles), 24),
        (offset_of!(RegistrySlot, late_cycles), 32),
    ];
    for (actual, specified) in segment_header
        .into_iter()
        .chain(control)
        .chain(ring)
        .chain(registry)
    {
        assert_eq!(actual, specified);
    }
}

#[test]
fn the_segment_layout_follows_from_its_configuration() {
    let layout = SegmentLayout::new(CONFIG).unwrap();
    assert_eq!(layout.control_offset(), 192);
    assert_eq!(layout.audio_offset(), 384);
    // 100 frames of f32 are 400 bytes, rounded up to 448.
    assert_eq!(layout.channel_stride(), 448);
    // 5 channels end at 384 + 5 * 448 = 2624.
    assert_eq!(layout.events_in_offset(), 2624);
    // 128 + 64 * 24 = 1664 bytes, ending at 4288, already aligned.
    assert_eq!(layout.events_out_offset(), 4288);
    // 128 + 16 * 24 = 512 bytes.
    assert_eq!(layout.size(), 4800);

    let no_rings = SegmentLayout::new(SegmentConfig {
        events_in_capacity: 0,
        events_out_capacity: 0,
        ..CONFIG
    })
    .unwrap();
    assert_eq!(no_rings.events_in_offset(), 0);
    assert_eq!(no_rings.events_out_offset(), 0);
    assert_eq!(no_rings.size(), 2624);
}

#[test]
fn a_segment_written_by_the_structures_reads_through_the_offsets() {
    let layout = SegmentLayout::new(CONFIG).unwrap();
    let mut mapping = Mapping::new(layout.size());
    // SAFETY: the mapping outlives the view.
    let view = unsafe { SegmentView::from_raw(mapping.base(), mapping.len(), layout) }.unwrap();
    let header = layout.header("synth à la Recife", 32).unwrap();
    // SAFETY: nobody else reads the mapping.
    unsafe { view.write_header(&header) };
    let control = view.control();
    control.cycle.store(7, Ordering::Release);
    control
        .frame_time
        .store(0x0102_0304_0506_0708, Ordering::Release);
    control.frames.store(96, Ordering::Release);
    control.done_cycle.store(6, Ordering::Release);
    control.bell.store(0xDEAD_BEEF, Ordering::Release);
    // SAFETY: output channel 1 has `max_frames` samples.
    unsafe { view.output(1).unwrap().as_ptr().add(5).write(0.25) };

    let bytes = mapping.bytes();
    assert_eq!(&bytes[0..8], b"MARESEG\0");
    assert_eq!(u16_at(bytes, 8), mare_protocol::VERSION_MAJOR);
    assert_eq!(u32_at(bytes, 12), 192);
    assert_eq!(u64_at(bytes, 16), layout.size());
    assert_eq!(u64_at(bytes, 24), 192);
    assert_eq!(u64_at(bytes, 32), 384);
    assert_eq!(u32_at(bytes, 40), 448);
    assert_eq!(u32_at(bytes, 44), 2);
    assert_eq!(u32_at(bytes, 48), 3);
    assert_eq!(u32_at(bytes, 52), 100);
    assert_eq!(u64_at(bytes, 56), 2624);
    assert_eq!(u64_at(bytes, 64), 4288);
    assert_eq!(u32_at(bytes, 72), 64);
    assert_eq!(u32_at(bytes, 76), 16);
    assert_eq!(u32_at(bytes, 80), 32);
    let name_len = u32_at(bytes, 84) as usize;
    assert_eq!(&bytes[88..88 + name_len], "synth à la Recife".as_bytes());

    let control = 192;
    assert_eq!(u64_at(bytes, control), 7);
    assert_eq!(u64_at(bytes, control + 8), 0x0102_0304_0506_0708);
    assert_eq!(u32_at(bytes, control + 16), 96);
    assert_eq!(u64_at(bytes, control + 64), 6);
    assert_eq!(u32_at(bytes, control + 128), 0xDEAD_BEEF);

    // Output channel 1 is the fourth channel (two inputs come first).
    let sample = 384 + 3 * 448 + 5 * 4;
    assert_eq!(
        f32::from_le_bytes(bytes[sample..sample + 4].try_into().unwrap()),
        0.25
    );
}

#[test]
fn a_segment_written_through_the_offsets_reads_through_the_structures() {
    let layout = SegmentLayout::new(CONFIG).unwrap();
    let mut mapping = Mapping::new(layout.size());
    {
        let bytes = mapping.bytes_mut();
        bytes[0..8].copy_from_slice(b"MARESEG\0");
        bytes[8..10].copy_from_slice(&0u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&9u16.to_le_bytes()); // a later minor version
        put_u32(bytes, 12, 192);
        put_u64(bytes, 16, 4800);
        put_u64(bytes, 24, 192);
        put_u64(bytes, 32, 384);
        put_u32(bytes, 40, 448);
        put_u32(bytes, 44, 2);
        put_u32(bytes, 48, 3);
        put_u32(bytes, 52, 100);
        put_u64(bytes, 56, 2624);
        put_u64(bytes, 64, 4288);
        put_u32(bytes, 72, 64);
        put_u32(bytes, 76, 16);
        put_u32(bytes, 80, 12);
        put_u32(bytes, 84, 4);
        bytes[88..92].copy_from_slice(b"mare");
        put_u64(bytes, 192 + 64, 41); // done_cycle
        // One event in the output ring: head = 1, entry 0.
        put_u32(bytes, 4288, 1);
        put_u64(bytes, 4288 + 128, 1000);
        put_u32(bytes, 4288 + 128 + 8, 0x2090_3C64);
    }
    let len = mapping.len();
    let base = mapping.base();
    // SAFETY: the mapping outlives the views.
    let probe = unsafe { SegmentView::from_raw(base, len, layout) }.unwrap();
    let header = probe.read_header();
    let read = SegmentLayout::validate(&header, len).unwrap();
    assert_eq!(read, layout);
    assert_eq!(header.name(), Some("mare"));
    assert_eq!(header.latency_frames, 12);
    assert_eq!(probe.control().done_cycle.load(Ordering::Acquire), 41);
    // SAFETY: this is the only consumer of the ring.
    let mut out = unsafe { mare_protocol::ring::RingConsumer::new(probe.events_out().unwrap()) };
    let event = out.pop().unwrap().unwrap();
    assert_eq!(event.time, 1000);
    assert_eq!(event.words[0], 0x2090_3C64);
    assert_eq!(out.pop(), Ok(None));
    assert_eq!(u32_at(mapping.bytes(), 4288 + 64), 1, "tail written back");
}

#[test]
fn a_header_that_disagrees_with_its_configuration_is_refused() {
    let layout = SegmentLayout::new(CONFIG).unwrap();
    let good = layout.header("x", 0).unwrap();
    let size = layout.size();
    assert_eq!(SegmentLayout::validate(&good, size), Ok(layout));

    type Spoil = fn(&mut SegmentHeader);
    let cases: [(Spoil, LayoutError); 9] = [
        (|h| h.magic[0] = b'X', LayoutError::BadMagic),
        (
            |h| h.version_major = 1,
            LayoutError::VersionMismatch { found: 1 },
        ),
        (
            |h| h.header_size = 128,
            LayoutError::Mismatch {
                field: "header_size",
            },
        ),
        (
            |h| h.segment_size += 64,
            LayoutError::Mismatch {
                field: "segment_size",
            },
        ),
        (
            |h| h.audio_offset = 0,
            LayoutError::Mismatch {
                field: "audio_offset",
            },
        ),
        // Pointing the output ring back over the audio must not pass.
        (
            |h| h.events_out_offset = 384,
            LayoutError::Mismatch {
                field: "events_out_offset",
            },
        ),
        (|h| h.input_channels = 1000, LayoutError::TooManyChannels),
        (|h| h.events_in_capacity = 3, LayoutError::RingCapacity),
        (
            |h| h.name_len = 65,
            LayoutError::Mismatch { field: "name_len" },
        ),
    ];
    for (spoil, expected) in cases {
        let mut header = good;
        spoil(&mut header);
        assert_eq!(SegmentLayout::validate(&header, size), Err(expected));
    }
    assert_eq!(
        SegmentLayout::validate(&good, size - 1),
        Err(LayoutError::Truncated {
            needed: size,
            available: size - 1
        })
    );
}

#[test]
fn configurations_out_of_range_have_no_layout() {
    let bad = [
        (
            SegmentConfig {
                max_frames: 0,
                ..CONFIG
            },
            LayoutError::FramesOutOfRange,
        ),
        (
            SegmentConfig {
                max_frames: 8193,
                ..CONFIG
            },
            LayoutError::FramesOutOfRange,
        ),
        (
            SegmentConfig {
                output_channels: 257,
                ..CONFIG
            },
            LayoutError::TooManyChannels,
        ),
        (
            SegmentConfig {
                events_out_capacity: 1 << 17,
                ..CONFIG
            },
            LayoutError::RingCapacity,
        ),
    ];
    for (config, expected) in bad {
        assert_eq!(SegmentLayout::new(config), Err(expected));
    }
    let largest = SegmentLayout::new(SegmentConfig {
        input_channels: 256,
        output_channels: 256,
        max_frames: 8192,
        events_in_capacity: 1 << 16,
        events_out_capacity: 1 << 16,
    })
    .unwrap();
    assert_eq!(largest.size() % 64, 0);
    assert!(
        SegmentLayout::new(CONFIG)
            .unwrap()
            .header(&"n".repeat(65), 0)
            .is_err()
    );
}

#[test]
fn a_misaligned_or_short_mapping_is_refused() {
    let layout = SegmentLayout::new(CONFIG).unwrap();
    let mut mapping = Mapping::new(layout.size() + 64);
    let base = mapping.base();
    // SAFETY: the mapping outlives the attempts, which fail before any use.
    unsafe {
        let shifted = NonNull::new(base.as_ptr().add(8)).unwrap();
        assert_eq!(
            SegmentView::from_raw(shifted, layout.size(), layout).err(),
            Some(LayoutError::Misaligned)
        );
        assert!(SegmentView::from_raw(base, layout.size() - 64, layout).is_err());
    }
}

#[test]
fn the_registry_reads_the_same_from_both_sides() {
    let layout = RegistryLayout::new(4).unwrap();
    assert_eq!(layout.size(), 192 + 4 * 64);
    let mut mapping = Mapping::new(layout.size());
    let len = mapping.len();
    // SAFETY: the mapping outlives the view.
    let view = unsafe { RegistryView::from_raw(mapping.base(), len, layout) }.unwrap();
    // SAFETY: nobody else reads it.
    unsafe { view.write_header(&layout.header(48_000, 64, 1024, 4242)) };
    view.control()
        .hub_state
        .store(hub_state::RUNNING, Ordering::Release);
    view.control().hub_bell.store(5, Ordering::Release);
    let slot = view.slot(2).unwrap();
    slot.state.store(slot_state::REQUESTED, Ordering::Release);
    slot.client_pid.store(77, Ordering::Release);
    slot.late_cycles.store(3, Ordering::Release);
    assert!(view.slot(4).is_none());

    let bytes = mapping.bytes();
    assert_eq!(&bytes[0..8], b"MAREHUB\0");
    assert_eq!(u64_at(bytes, 16), layout.size());
    assert_eq!(u64_at(bytes, 24), 64);
    assert_eq!(u64_at(bytes, 32), 192);
    assert_eq!(u32_at(bytes, 40), 4);
    assert_eq!(u32_at(bytes, 44), 64);
    assert_eq!(u32_at(bytes, 48), 48_000);
    assert_eq!(u32_at(bytes, 52), 64);
    assert_eq!(u32_at(bytes, 56), 1024);
    assert_eq!(u32_at(bytes, 60), 4242);
    assert_eq!(u32_at(bytes, 64), hub_state::RUNNING);
    assert_eq!(u32_at(bytes, 128), 5);
    let slot2 = 192 + 2 * 64;
    assert_eq!(u32_at(bytes, slot2), slot_state::REQUESTED);
    assert_eq!(u32_at(bytes, slot2 + 8), 77);
    assert_eq!(u64_at(bytes, slot2 + 32), 3);

    let header = view.read_header();
    assert_eq!(RegistryLayout::validate(&header, len), Ok(layout));
    let mut wrong = header;
    wrong.period_frames = 2048;
    assert_eq!(
        RegistryLayout::validate(&wrong, len),
        Err(LayoutError::Mismatch {
            field: "max_frames"
        })
    );
    assert_eq!(RegistryLayout::new(0), Err(LayoutError::SlotCount));
    assert_eq!(RegistryLayout::new(1025), Err(LayoutError::SlotCount));
}
