# MARÉ: design

**MARÉ** (Multi-device Audio Realtime Exchange) is an open protocol, and a
hub that implements it, for moving audio and MIDI between applications and
audio devices with low latency. *Maré* is Portuguese for the tide: the
mangrove lives by it, filling and emptying in cycles, as audio buffers do.
It belongs to the ecosystem of the
[Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte),
from Recife.

Status: milestone 2 is built (the shared-memory layout, the client library,
and a hub tested against fake devices), and milestone 3 is under way (a
WASAPI device, output and duplex input, run by hand on one device so far); the rest is
design. This document is
the specification the code follows, and it changes as the code proves or
disproves it. Where the code has changed it, the text says so. The layout
itself, byte by byte, is in [LAYOUT.md](LAYOUT.md); what the code still owes
is in [TECH_DEBT.md](TECH_DEBT.md).

## Why

On Windows, music software has standardised on ASIO, a protocol from
Steinberg between an application and a device's driver (ASIO is a trademark
of Steinberg Media Technologies GmbH; see [Trademarks](#trademarks)). ASIO is fast
because the driver underneath is fast, but the protocol itself carries
limits from the 1990s:

- **One application at a time.** Opening a session in a DAW silences every
  other program on the same device.
- **One device at a time.** Two interfaces, or an interface and a USB
  microphone, cannot be used together without a vendor's own aggregation,
  and the generic wrappers that try (ASIO4ALL) click as the clocks drift.
- **No hot-plug.** Unplugging a device, or changing the buffer size, resets
  the whole session.
- **MIDI apart.** Audio and MIDI reach the application through unrelated
  APIs with unrelated clocks.
- **Latency on trust.** A driver reports its latency; nothing measures it.
- **A restrictive licence.** The interface definitions come with Steinberg's
  SDK, under the GPLv3 or a commercial licence, and the name is a trademark.

MARÉ keeps what makes ASIO fast (the application writes straight into the
buffers the device plays, on a schedule the device sets) and removes those
limits. It is open, under MIT or Apache-2.0, so that any program and any
language can implement either side.

## What MARÉ is not

- **Not a kernel driver.** MARÉ runs in user mode, on top of the drivers a
  system already has (on Windows, WASAPI over WaveRT; CoreAudio on macOS;
  PipeWire or ALSA on Linux). A kernel driver needs a signing certificate,
  Microsoft's attestation and a different risk profile (a bug stops the
  machine), and it is out of scope.
- **Not lower latency than the hardware allows.** The floor is set by the
  device's driver, its USB or PCIe transfers and its converters. MARÉ's
  promise is to add nothing to that floor beyond one period, to say exactly
  what the latency is, and to add what ASIO cannot do.
- **Not a virtual sound card for every program.** Programs that only speak
  WASAPI or WDM keep using the system's mixer. Reaching them as a device
  would need a kernel driver (see above).

## Prior art

What each existing system teaches, read from its public documentation only.
No code from any of them is read or copied, and in particular none from GPL
or LGPL projects (JACK, PipeWire's GPL parts, ASIO4ALL).

| System | What it gets right | What MARÉ does differently |
|---|---|---|
| ASIO | Direct, period-driven buffer exchange; one callback a period | Many clients, many devices, MIDI on the same clock, open licence |
| WASAPI exclusive, event-driven | The lowest latency the in-box stack offers | Used underneath as a device backend |
| WASAPI shared, `IAudioClient3` | Small periods without exclusive mode (Windows 10+) | Used underneath when exclusivity is not wanted |
| JACK, PipeWire | A graph of clients in shared memory, run once a period | Windows first, MIDI 2.0 native, drift-corrected aggregation as a core feature |
| CoreAudio aggregate devices | Several devices as one, with drift correction | The same on Windows and Linux |
| Microsoft's new USB Audio Class 2 driver (with Qualcomm and Yamaha, preview in 2026) | Low latency for class-compliant USB devices, in the box | A better floor for MARÉ to sit on, not a competitor |

## Architecture

```
 application ──┐                                   ┌── device A (master clock)
 application ──┼── shared memory ── MARÉ hub ──────┼── device B (resampled)
 application ──┘    (per client)    one cycle      └── device C (resampled)
                                    a period
```

### The hub

One hub runs per user session. It opens the devices it is configured to use,
picks one of them as the **master clock**, and runs one **cycle** per period
of that device on a real-time thread (on Windows, registered with MMCSS as
"Pro Audio", which is built; on other systems not yet). Each cycle:

1. reads the input of every device for this period (devices other than the
   master go through an adaptive resampler, below);
2. wakes the clients in the order the graph needs, each with a deadline;
3. mixes what the clients wrote into the device outputs;
4. hands the outputs to the devices.

The hub never waits on a client past its deadline. A client that misses one
loses its output for that cycle, which is counted and reported, and the hub
goes on: one stalled program never glitches the others. A client still
working on an earlier cycle when the next begins is not woken, and its
buffers are not touched, until it finishes; those cycles count as missed
too. A client that dies is detected (its process handle signals) and its
resources are freed.

The hub is two halves that never wait for each other. The **engine** runs
the cycles on the real-time thread and never allocates, frees or locks. The
**housekeeper**, on an ordinary thread, adopts the clients that connect and
lets go of those that leave or die. Each client's resources belong to one
half at a time, handed over with an atomic phase; the engine lets go of a
client at the start of a cycle, and only then does the housekeeper free it,
so freeing never happens on the real-time thread. The housekeeper sleeps on a doorbell
of its own, which clients ring when they connect or leave and the engine
rings when it lets a client go.

### The clock

Time in MARÉ is a 64-bit count of frames of the master clock since the hub
started, mapped to the system's monotonic clock at every cycle. Everything
carries a timestamp in those frames: audio periods, MIDI events, transport.
MIDI travels as MIDI 2.0 Universal MIDI Packets, each with the frame it
belongs to, in the same cycle as the audio around it (MIDI 1.0 devices are
translated at the edge). This is the one timestamped event stream of
Manguebyte's ADR 0005, carried between processes.

### Several devices

A device that is not the master runs on its own clock, which drifts from the
master's by parts per million. The hub drives it on a thread of its own and
joins it to the cycle through a queue in each direction, with a
high-quality adaptive resampler whose ratio a controller sets from the
queue's level ([ADR 0001](adr/0001-drift-correction-by-queue-level.md)), so
that the devices behave as one without clicks. Its channels follow the
master's. The drift estimate and the latency each queue adds are reported.
A device will be able to join or leave while the hub runs, its ports fading
in or out while the other devices carry on; today a follower whose device
fails falls silent and the others carry on.

### Clients and shared memory

A client connects through the hub's **registry**, a small segment of shared
memory the hub publishes under a name derived from its own: it says how the
hub runs and holds one slot per client. The client takes a free slot with a
compare-and-swap, creates a segment of its own, and asks the hub to adopt it;
the hub validates the segment and accepts or refuses it with a reason. (The
first draft planned a named pipe for this; a registry needs no server thread
and no message format, and works the same on every system.) The client's
segment holds:

- a **control block** of atomics: the cycle number, the client's deadline,
  its state, counters of late cycles;
- **audio buffers** for its ports, one period each: the hub fills a client's
  inputs before waking it and reads its outputs after. The first draft
  double-buffered them; one buffer is enough, because the hub writes inputs
  only to an idle client and reads outputs only of a client that has marked
  the current cycle done, so the two sides never touch a buffer at once;
- **event rings** of timestamped UMP packets, in and out;
- the **graph view** it needs: which ports exist and their latency.

Nothing in that segment is a pointer: offsets only, so that a 32-bit and a
64-bit process, or two programs in different languages, share it. The
layout is fixed by this specification (C layout, little-endian, explicit
sizes and alignment) and versioned.

Waking is a **doorbell**: a counter in shared memory that one side adds to
and the other waits on, plus one wake object per platform so that the
waiter can sleep: a named auto-reset event on Windows and a futex on the
counter itself on Linux (both built), a semaphore on macOS (planned; until
then macOS polls the counter). The hub waits for its clients with a high-resolution waitable
timer, so that a deadline under a millisecond ends on time. A client's audio
thread waits on its doorbell, runs, marks the cycle done with a release
store, rings the hub's doorbell and waits again. No locks, no allocation and
no system call besides the wait and the signal on that path; a test with a
counting allocator holds both sides to it.

The segment of a client that is not trusted is checked before use: every
offset in its header must be the one the hub computes from the header's
channel counts, period and ring capacities, so a header that points outside
its segment, or makes two blocks overlap, is refused.

### Ports and the graph

Every device channel and every client port is a port, audio or event.
Connections join an output port to input ports; several outputs into one
input are summed. Clients run in an order derived from the connections, and
clients with no path between them may run in parallel on separate cores.
In the first version the graph is simple: each client's outputs are summed
into the devices' outputs, and each client reads the devices' inputs.

### Latency, said exactly

Every path reports its latency in frames, split into its parts: the device's
own (as its driver reports it), the hub's one period, any resampler's, and
any client's declared latency. A tool in the hub measures a device's real
round trip through a loopback cable, and marks the reported figure as
measured.

## Bridges

- **Manguebyte.** The first client library is a backend of
  `manguebyte-devices`, so every program built on Manguebyte (its standalone
  plugins, Aratu, Parabolic) speaks MARÉ without changing its code (built:
  `mare-manguebyte`, whose `MareBackend` a `DeviceManager` takes through
  `with_audio_backend`; Manguebyte's standalone runs on such a manager
  through `Standalone::start_on`).
- **Existing DAWs.** DAWs on Windows speak ASIO. An optional bridge, a
  driver DLL that a DAW loads as any ASIO driver and that speaks MARÉ
  underneath, lets them use the hub. Because it implements Steinberg's
  interface, it is a separate project under the GPLv3, like
  `manguebyte-pro-io`, and its name does not contain "ASIO" (a Steinberg
  trademark). The protocol and the hub stay MIT or Apache-2.0.
- **A C API.** A small C library implements the client side, so that any
  language can join without the Rust code (built: `mare-capi`, with
  `include/mare_client.h` and a C++ layer, `include/mare_client.hpp`).

## Platforms

1. **Windows** first: WASAPI exclusive and event-driven, or shared with
   `IAudioClient3` small periods, per device.
2. **Linux**: ALSA directly, or as a PipeWire client.
3. **macOS**: CoreAudio.

The protocol is the same everywhere; only the device backends and the wake
primitive differ.

## Security

The hub runs as the user, never as a service with more rights. Its channel
and shared memory are created with an access list for that user alone, so
another user on the same machine cannot connect (built on Windows, where
each mapping and event gets a protected access list with a single entry for
the current user; elsewhere shared memory is created with mode `0600`). There is no network
transport in the first version. Audio over a network (in the spirit of
Dante or AES67) is a possible later extension, on the same clock model.

## How it is verified

As in Manguebyte, nothing is claimed before its test exists.

- The hub runs against fake devices with injected clocks, so cycles, drift,
  hot-plug and late or dying clients are tested deterministically on every
  system in CI.
- The shared-memory layout is tested from both sides, and from a second
  language through the C API.
- Drift correction is measured: two fake devices whose clocks differ by a
  known amount, over hours of simulated time, with no clicks (no
  discontinuity above a threshold) and a bounded latency.
- Stress tests: a hundred clients, clients that stall, crash or leave
  mid-cycle, devices that vanish.
- Real devices play silence in automated tests. Sound through real hardware
  is tried by hand, and each such session is written down.

## Milestones

1. This specification, and the name checked against trademark registers
   (the specification is written; the trademark search is not done).
2. The shared-memory layout and the client library, tested against a hub
   with fake devices (built).
3. The hub on Windows with one device (WASAPI), one client, then many
   (under way: output and duplex input, in exclusive and shared mode, are
   built and have run by hand on the laptop's Realtek device, recorded in
   [HARDWARE.md](HARDWARE.md); the UMC404HD and a loopback measurement of
   latency are next).
4. Several devices with drift correction; hot-plug (under way: the
   resampler, the controller and an aggregate device of resampled
   followers are built and tested on fake devices,
   [ADR 0001](adr/0001-drift-correction-by-queue-level.md); followers on
   WASAPI endpoints and hot-plug are next).
5. MIDI 2.0 on the same clock.
6. The Manguebyte backend (under way: `mare-manguebyte` is built, and a
   plugin played by Manguebyte's standalone has run through it on MARÉ's
   own hubs; a hub program for real devices, and Caecilia playing through
   it, are next).
7. The DAW bridge (separate, GPLv3).
8. Linux and macOS backends.

## Open questions

- The trademark search for "MARÉ" / "MARE" in the INPI (Brazil) and the
  USPTO, in the classes for software (9) and services (42).
- Whether the master clock can change while the hub runs (the master device
  is unplugged) without a gap, or only with a short fade.
- How the graph is configured: by each client, by a separate patchbay
  program, or both.
- Whether clients may share one process with the hub (in-process plugins of
  the hub itself), for the lowest overhead.

## Trademarks

ASIO is a trademark and software of Steinberg Media Technologies GmbH. MARÉ
is not affiliated with, endorsed by or derived from Steinberg or ASIO: it
uses no part of the ASIO SDK, and its name and its parts' names do not
contain "ASIO". The protocol is named here only to compare it with MARÉ.

## Licence

The specification, the hub and the client library are under MIT or
Apache-2.0, at the user's option. The DAW bridge, which implements
Steinberg's interface, is a separate project under the GPLv3.
