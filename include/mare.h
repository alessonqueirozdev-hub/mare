/*
 * mare.h: the shared-memory layout of MARÉ, layout version 0.1, in C.
 *
 * MIT OR Apache-2.0. This header is the C form of docs/LAYOUT.md and of the
 * Rust crate mare-protocol; crates/mare-protocol/tests/c_layout.rs compiles
 * a C program with it and checks that its offsets and its layout
 * computation agree with the Rust ones.
 *
 * Rules (see docs/LAYOUT.md):
 *
 * - all integers are little-endian; there are no pointers, only offsets
 *   from the start of a segment;
 * - every block starts on a multiple of 64 bytes, and segments are mapped
 *   at such an address;
 * - fields marked ATOMIC are shared between processes while they run: read
 *   them with an acquire load and write them with a release store (C11
 *   <stdatomic.h> on a cast pointer, or the compiler's builtins). They are
 *   declared as plain integers, since not every C compiler accepts
 *   _Atomic; define MARE_ATOMIC to _Atomic to declare them so;
 * - other fields are written once, before a segment is published, and a
 *   reader copies and checks them once.
 *
 * Requires C11 or C++11 (static assertions). Every 64-bit field sits at a multiple of 8
 * after explicit padding, so the offsets are the same where uint64_t is only
 * 4-aligned (32-bit x86); the checks at the end hold the header to them.
 */

#ifndef MARE_H
#define MARE_H

#include <stddef.h>
#include <stdint.h>

#ifndef MARE_ATOMIC
#define MARE_ATOMIC
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Versions. A reader refuses another major version; minor versions only give
 * meaning to reserved bytes. */
#define MARE_VERSION_MAJOR 0
#define MARE_VERSION_MINOR 1

#define MARE_BLOCK_ALIGN 64

/* Limits. */
#define MARE_MAX_CHANNELS 256u
#define MARE_MAX_FRAMES 8192u
#define MARE_MAX_RING_CAPACITY 65536u
#define MARE_MAX_SLOTS 1024u
#define MARE_NAME_CAPACITY 64u

/* The first eight bytes of each kind of segment. */
#define MARE_REGISTRY_MAGIC "MAREHUB"
#define MARE_SEGMENT_MAGIC "MARESEG"

/* ---- The registry: one per hub ---------------------------------------- */

/* registry control: hub_state */
#define MARE_HUB_STARTING 0u
#define MARE_HUB_RUNNING 1u
#define MARE_HUB_STOPPED 2u

/* registry slot: state */
#define MARE_SLOT_FREE 0u
#define MARE_SLOT_CLAIMED 1u
#define MARE_SLOT_REQUESTED 2u
#define MARE_SLOT_ACCEPTED 3u
#define MARE_SLOT_REJECTED 4u
#define MARE_SLOT_LEAVING 5u

/* registry slot: reject_reason */
#define MARE_REJECT_NONE 0u
#define MARE_REJECT_NO_SEGMENT 1u
#define MARE_REJECT_BAD_SEGMENT 2u
#define MARE_REJECT_FRAMES 3u
#define MARE_REJECT_NO_PROCESS 4u
#define MARE_REJECT_NO_DOORBELL 5u
#define MARE_REJECT_FULL 6u

typedef struct mare_registry_header {
    uint8_t magic[8];           /* MARE_REGISTRY_MAGIC and a NUL */
    uint16_t version_major;
    uint16_t version_minor;
    uint32_t header_size;       /* 64 */
    uint64_t segment_size;
    uint64_t control_offset;    /* 64 */
    uint64_t slots_offset;      /* 192 */
    uint32_t slot_count;        /* 1 to MARE_MAX_SLOTS */
    uint32_t slot_size;         /* 64 */
    uint32_t sample_rate;
    uint32_t period_frames;
    uint32_t max_frames;
    uint32_t hub_pid;
} mare_registry_header;

typedef struct mare_registry_control {
    MARE_ATOMIC uint32_t hub_state; /* MARE_HUB_* */
    uint8_t pad0[4];
    MARE_ATOMIC uint64_t cycle;     /* the last cycle the hub started */
    uint8_t pad1[48];
    MARE_ATOMIC uint32_t hub_bell;  /* the hub's doorbell */
    uint8_t pad2[60];
} mare_registry_control;

typedef struct mare_registry_slot {
    MARE_ATOMIC uint32_t state;          /* MARE_SLOT_* */
    MARE_ATOMIC uint32_t generation;
    MARE_ATOMIC uint32_t client_pid;
    MARE_ATOMIC uint32_t reject_reason;  /* MARE_REJECT_* */
    MARE_ATOMIC uint64_t segment_size;
    MARE_ATOMIC uint64_t cycles;         /* finished in time; hub writes */
    MARE_ATOMIC uint64_t late_cycles;    /* missed; hub writes */
    uint8_t reserved[24];
} mare_registry_slot;

#define MARE_REGISTRY_CONTROL_OFFSET 64u
#define MARE_REGISTRY_SLOTS_OFFSET 192u

/* Bytes in a registry of `slot_count` slots. */
static inline uint64_t mare_registry_size(uint32_t slot_count)
{
    return MARE_REGISTRY_SLOTS_OFFSET + (uint64_t)slot_count * 64u;
}

/* ---- The client segment: one per client ------------------------------- */

/* control: hub_flags and client_flags */
#define MARE_HUB_FLAG_STOP 1u
#define MARE_CLIENT_FLAG_LEAVING 1u
#define MARE_CLIENT_FLAG_READY 2u /* waiting for cycles; until then the hub does not wake it */

typedef struct mare_segment_header {
    uint8_t magic[8];             /* MARE_SEGMENT_MAGIC and a NUL */
    uint16_t version_major;
    uint16_t version_minor;
    uint32_t header_size;         /* 192 */
    uint64_t segment_size;
    uint64_t control_offset;      /* 192 */
    uint64_t audio_offset;        /* 384 */
    uint32_t channel_stride;      /* bytes between audio channels */
    uint32_t input_channels;
    uint32_t output_channels;
    uint32_t max_frames;
    uint64_t events_in_offset;    /* 0 if there is no ring into the client */
    uint64_t events_out_offset;   /* 0 if there is no ring out of it */
    uint32_t events_in_capacity;
    uint32_t events_out_capacity;
    uint32_t latency_frames;
    uint32_t name_len;
    uint8_t name[MARE_NAME_CAPACITY]; /* UTF-8, not NUL-terminated */
    uint8_t reserved[40];
} mare_segment_header;

typedef struct mare_control_block {
    /* Written by the hub. */
    MARE_ATOMIC uint64_t cycle;       /* release, after the inputs */
    MARE_ATOMIC uint64_t frame_time;
    MARE_ATOMIC uint32_t frames;
    MARE_ATOMIC uint32_t hub_flags;   /* MARE_HUB_FLAG_* */
    MARE_ATOMIC uint64_t budget_ns;
    uint8_t pad0[32];
    /* Written by the client. */
    MARE_ATOMIC uint64_t done_cycle;  /* release, after the outputs */
    MARE_ATOMIC uint32_t client_flags; /* MARE_CLIENT_FLAG_* */
    uint8_t pad1[52];
    /* The client's doorbell: the hub adds one to wake it. */
    MARE_ATOMIC uint32_t bell;
    uint8_t pad2[60];
} mare_control_block;

/* A ring of events: this header, then `capacity` events. */
typedef struct mare_ring_header {
    MARE_ATOMIC uint32_t head;   /* events written; producer */
    uint8_t pad0[60];
    MARE_ATOMIC uint32_t tail;   /* events read; consumer */
    uint8_t pad1[60];
} mare_ring_header;

/* One Universal MIDI Packet with the master clock's frame it belongs to. */
typedef struct mare_ump_event {
    uint64_t time;
    uint32_t words[4];
} mare_ump_event;

/* Where everything is in a client segment. */
typedef struct mare_segment_layout {
    uint64_t control_offset;
    uint64_t audio_offset;
    uint64_t channel_stride;
    uint64_t events_in_offset;
    uint64_t events_out_offset;
    uint64_t size;
} mare_segment_layout;

static inline uint64_t mare_align_block(uint64_t value)
{
    return (value + (MARE_BLOCK_ALIGN - 1)) & ~(uint64_t)(MARE_BLOCK_ALIGN - 1);
}

static inline uint64_t mare_ring_bytes(uint32_t capacity)
{
    return 128u + (uint64_t)capacity * 24u;
}

static inline int mare_valid_capacity(uint32_t capacity)
{
    return capacity == 0
        || ((capacity & (capacity - 1)) == 0 && capacity <= MARE_MAX_RING_CAPACITY);
}

/* Computes the layout of a client segment from its five numbers. Returns 0,
 * or -1 if they are out of range. A hub computes this itself and refuses a
 * header whose offsets differ. */
static inline int mare_segment_layout_of(uint32_t input_channels,
                                         uint32_t output_channels,
                                         uint32_t max_frames,
                                         uint32_t events_in_capacity,
                                         uint32_t events_out_capacity,
                                         mare_segment_layout *out)
{
    uint64_t end;
    if (input_channels > MARE_MAX_CHANNELS || output_channels > MARE_MAX_CHANNELS)
        return -1;
    if (max_frames == 0 || max_frames > MARE_MAX_FRAMES)
        return -1;
    if (!mare_valid_capacity(events_in_capacity) || !mare_valid_capacity(events_out_capacity))
        return -1;
    out->control_offset = 192u;
    out->audio_offset = 384u;
    out->channel_stride = mare_align_block((uint64_t)max_frames * 4u);
    end = out->audio_offset
        + ((uint64_t)input_channels + output_channels) * out->channel_stride;
    out->events_in_offset = 0;
    if (events_in_capacity > 0) {
        out->events_in_offset = end;
        end = mare_align_block(end + mare_ring_bytes(events_in_capacity));
    }
    out->events_out_offset = 0;
    if (events_out_capacity > 0) {
        out->events_out_offset = end;
        end = mare_align_block(end + mare_ring_bytes(events_out_capacity));
    }
    out->size = end;
    return 0;
}

/* The number of 32-bit words in the packet whose first word is `word0`. */
static inline unsigned mare_ump_packet_words(uint32_t word0)
{
    switch (word0 >> 28) {
    case 0x0: case 0x1: case 0x2: case 0x6: case 0x7:
        return 1;
    case 0x3: case 0x4: case 0x8: case 0x9: case 0xA:
        return 2;
    case 0xB: case 0xC:
        return 3;
    default:
        return 4;
    }
}

/* ---- The layout, checked when this header is compiled ----------------- */

#ifdef __cplusplus
#define MARE_CHECK(what, expr) static_assert(expr, "mare.h: " what)
#else
#define MARE_CHECK(what, expr) _Static_assert(expr, "mare.h: " what)
#endif

MARE_CHECK("registry header size", sizeof(mare_registry_header) == 64);
MARE_CHECK("registry control size", sizeof(mare_registry_control) == 128);
MARE_CHECK("registry slot size", sizeof(mare_registry_slot) == 64);
MARE_CHECK("segment header size", sizeof(mare_segment_header) == 192);
MARE_CHECK("control block size", sizeof(mare_control_block) == 192);
MARE_CHECK("ring header size", sizeof(mare_ring_header) == 128);
MARE_CHECK("event size", sizeof(mare_ump_event) == 24);

MARE_CHECK("hub_pid", offsetof(mare_registry_header, hub_pid) == 60);
MARE_CHECK("slot_count", offsetof(mare_registry_header, slot_count) == 40);
MARE_CHECK("cycle", offsetof(mare_registry_control, cycle) == 8);
MARE_CHECK("hub_bell", offsetof(mare_registry_control, hub_bell) == 64);
MARE_CHECK("segment_size", offsetof(mare_registry_slot, segment_size) == 16);
MARE_CHECK("late_cycles", offsetof(mare_registry_slot, late_cycles) == 32);

MARE_CHECK("segment_size", offsetof(mare_segment_header, segment_size) == 16);
MARE_CHECK("channel_stride", offsetof(mare_segment_header, channel_stride) == 40);
MARE_CHECK("events_in_offset", offsetof(mare_segment_header, events_in_offset) == 56);
MARE_CHECK("events_out_capacity", offsetof(mare_segment_header, events_out_capacity) == 76);
MARE_CHECK("name", offsetof(mare_segment_header, name) == 88);
MARE_CHECK("reserved", offsetof(mare_segment_header, reserved) == 152);

MARE_CHECK("frames", offsetof(mare_control_block, frames) == 16);
MARE_CHECK("budget_ns", offsetof(mare_control_block, budget_ns) == 24);
MARE_CHECK("done_cycle", offsetof(mare_control_block, done_cycle) == 64);
MARE_CHECK("client_flags", offsetof(mare_control_block, client_flags) == 72);
MARE_CHECK("bell", offsetof(mare_control_block, bell) == 128);

MARE_CHECK("tail", offsetof(mare_ring_header, tail) == 64);
MARE_CHECK("words", offsetof(mare_ump_event, words) == 8);

#undef MARE_CHECK

#ifdef __cplusplus
}
#endif

#endif /* MARE_H */
