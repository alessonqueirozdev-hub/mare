# MARÉ: the shared-memory layout

This is the byte-level layout of layout version 0.2, as `mare-protocol`
implements it in Rust and [`include/mare.h`](../include/mare.h) in C.
`crates/mare-protocol/tests/layout.rs` checks the Rust structures against
these tables, and reads and writes segments through these offsets alone, as
a program in another language would. `tests/c_layout.rs` compiles a C
program with `mare.h` (whose static checks hold every size and offset to
these tables), runs the header's own layout computation over 360
configurations, valid and not, and requires the same results as the Rust
one; it also compiles the header as C++.

All integers are little-endian. Every block starts on a multiple of 64 bytes,
and a segment is mapped at an address that is a multiple of 64. Nothing is a
pointer. Fields marked *atomic* are written by one side and read by the other
with acquire and release ordering; the rest are written once, before the
segment is published, and a reader copies and checks them once.

A reader refuses a segment whose major version differs from its own, and
accepts any minor version (minor versions only give meaning to reserved
bytes).

## The registry

One per hub, created by the hub. Its name is derived from the hub's name:
`Local\mare.<hub>` on Windows, `/mare.<hub>` elsewhere.

| Block | Offset | Size |
|---|---|---|
| header | 0 | 64 |
| control | 64 | 128 |
| slots | 192 | 64 each |

Header:

| Offset | Type | Field |
|---|---|---|
| 0 | `u8[8]` | magic, `MAREHUB\0` |
| 8 | `u16` | version major (0) |
| 10 | `u16` | version minor (2) |
| 12 | `u32` | header size (64) |
| 16 | `u64` | registry size |
| 24 | `u64` | control offset (64) |
| 32 | `u64` | slots offset (192) |
| 40 | `u32` | slot count, 1 to 1024 |
| 44 | `u32` | slot size (64) |
| 48 | `u32` | sample rate |
| 52 | `u32` | frames per cycle |
| 56 | `u32` | longest cycle, in frames (at most 8192) |
| 60 | `u32` | the hub's process id |

Control:

| Offset | Type | Field |
|---|---|---|
| 0 | atomic `u32` | hub state: 0 starting, 1 running, 2 stopped |
| 8 | atomic `u64` | the last cycle the hub started |
| 16 | atomic `u32` | the housekeeper's doorbell (since 0.2) |
| 64 | atomic `u32` | the hub's doorbell (cycles finished) |

A client reads the hub state first, and trusts the header only once it is 1.

Slot:

| Offset | Type | Field | Written by |
|---|---|---|---|
| 0 | atomic `u32` | state (below) | both |
| 4 | atomic `u32` | generation | hub |
| 8 | atomic `u32` | client's process id | client |
| 12 | atomic `u32` | reason for a rejection | hub |
| 16 | atomic `u64` | size of the client's segment | client |
| 24 | atomic `u64` | cycles finished in time | hub |
| 32 | atomic `u64` | cycles missed | hub |

Slot states: 0 free, 1 claimed, 2 requested, 3 accepted, 4 rejected,
5 leaving. A client rings the housekeeper's doorbell (adds one, and on Windows
signals the event `Local\mare.<hub>.keeper`) after storing requested or
leaving, so the hub adopts or frees it at once. A client takes a free slot by compare-and-swap to claimed, creates
its segment, stores its process id and segment size, and stores requested.
The hub answers accepted or rejected. A client that leaves stores leaving.
The hub frees a slot (adds one to its generation, then stores free) after a
client leaves, after a rejection, and when a client's process ends.

Reasons for a rejection: 1 the segment could not be opened, 2 its header does
not follow this layout, 3 its buffers are shorter than the longest cycle,
4 the client's process cannot be watched, 5 its doorbell cannot be opened,
6 the hub is full.

## The client segment

One per client, created by the client, named after the hub, the slot and the
slot's generation: `Local\mare.<hub>.<slot>.<generation>` on Windows,
`/mare.<hub>.<slot>.<generation>` elsewhere, with the slot and generation in
lower-case hexadecimal.

| Block | Size |
|---|---|
| header | 192 |
| control | 192 |
| audio: input channels, then output channels | channel stride each |
| ring of events into the client, if any | 128 + 24 × capacity, rounded up to 64 |
| ring of events out of the client, if any | 128 + 24 × capacity, rounded up to 64 |

Every offset follows from five numbers: input channels and output channels
(at most 256 each), the longest period in frames (1 to 8192), and the two
ring capacities (zero, or a power of two up to 65 536). The channel stride is
4 × the longest period, rounded up to 64. The hub computes the layout itself
and refuses a header whose offsets differ from its own.

Header:

| Offset | Type | Field |
|---|---|---|
| 0 | `u8[8]` | magic, `MARESEG\0` |
| 8 | `u16` | version major (0) |
| 10 | `u16` | version minor (2) |
| 12 | `u32` | header size (192) |
| 16 | `u64` | segment size |
| 24 | `u64` | control offset (192) |
| 32 | `u64` | audio offset (384) |
| 40 | `u32` | channel stride, in bytes |
| 44 | `u32` | input channels |
| 48 | `u32` | output channels |
| 52 | `u32` | longest period, in frames |
| 56 | `u64` | offset of the ring into the client, or 0 |
| 64 | `u64` | offset of the ring out of the client, or 0 |
| 72 | `u32` | capacity of the ring into the client |
| 76 | `u32` | capacity of the ring out of the client |
| 80 | `u32` | latency the client declares, in frames |
| 84 | `u32` | bytes of the name in use |
| 88 | `u8[64]` | the client's name, UTF-8 |
| 152 | `u8[40]` | reserved, zero |

Control (the first line is written by the hub, the second by the client):

| Offset | Type | Field |
|---|---|---|
| 0 | atomic `u64` | the cycle the client is woken for, from 1 |
| 8 | atomic `u64` | the master clock's frame at this cycle's first frame |
| 16 | atomic `u32` | frames in this cycle |
| 20 | atomic `u32` | hub flags: bit 0, stop |
| 24 | atomic `u64` | the client's budget, in nanoseconds from being woken |
| 64 | atomic `u64` | the last cycle the client finished |
| 72 | atomic `u32` | client flags: bit 0, leaving; bit 1, ready |
| 128 | atomic `u32` | the client's doorbell |

Audio is 32-bit float, one buffer per channel, not interleaved, holding one
period.

## A cycle

1. The hub writes the client's input channels, the frame time, the frame
   count and the budget, then stores the cycle number (release), adds one to
   the client's doorbell, and signals the client's wake object (on Windows,
   an auto-reset event named like the segment with `.go` appended).
2. The client wakes, reads the cycle number (acquire), reads its inputs,
   writes all its output channels, stores the cycle number as the last cycle
   it finished (release), adds one to the hub's doorbell and signals the
   hub's wake object (on Windows, `Local\mare.<hub>.bell`).
3. The hub mixes the outputs of every client that finished this cycle. A
   client that did not finish by its deadline is counted as late and
   contributes nothing.

A client is not woken until it sets its ready flag, which it does when its
audio thread first waits for a cycle: it is adopted while it is still
connecting, and a cycle it could not have run is not counted against it. A
client that is still working on an earlier cycle is not woken, and its
buffers are not touched, until it finishes. This is why one buffer per channel
is enough: the hub writes inputs only to an idle client, and reads outputs
only of a client that has finished the current cycle.

## Event rings

| Offset | Type | Field | Written by |
|---|---|---|---|
| 0 | atomic `u32` | head: events written, modulo 2³² | producer |
| 64 | atomic `u32` | tail: events read, modulo 2³² | consumer |
| 128 | entries | capacity × 24 bytes | producer |

An entry is a `u64` frame time (the same clock as the audio) and four `u32`
words of a Universal MIDI Packet, unused words zero. The entry for event
number *n* is at 128 + 24 × (*n* mod capacity). The hub produces into the
ring into the client and consumes the ring out of it. A consumer that finds
head − tail greater than the capacity treats the ring as corrupt and
discards its contents rather than reading out of bounds.
