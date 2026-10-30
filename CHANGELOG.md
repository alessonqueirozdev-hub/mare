# Changelog

What changed in MARÉ, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break the protocol and the API.

## Unreleased

### WASAPI (`mare-wasapi`)

- `WasapiDevice`, a `mare_hub::Device` for a WASAPI render endpoint,
  event-driven, in exclusive mode (the device's own format, negotiated
  from 32-bit float down to 16-bit integer, and its smallest period, with
  the realignment a driver may ask for) or in shared mode with
  `IAudioClient3`'s small periods. The cycle thread waits on the device's
  event and hands it each period's mix, converted to the device's format,
  without allocating. `render_endpoints` lists outputs without opening them;
  `Latency` gives the period, the buffer and the driver's stream latency.
- Duplex: `WasapiDevice::open_duplex` opens a capture endpoint beside the
  output, in the same mode and at the same sample rate, and
  `capture_endpoints` lists inputs. The output paces the cycle; each cycle
  takes the input's packets into a `fifo::CaptureFifo` and hands the hub
  one period. The queue starts with a period of cushion, places packets by
  their position on the device's clock (in exclusive mode the Realtek input
  hands every packet over twice; repeats are left out, gaps become
  silence; only in exclusive mode, because in shared mode positions count
  the device's frames, 441 for every 480 of the stream on the Realtek
  microphone), and keeps its backlog bounded, counting every correction.
  `CaptureMonitor` publishes the counters for another thread to read live.
  `examples/meter.rs` prints an input's level as a client reads it, with a
  timeline of what the input delivered.
- `format`: conversion between the hub's floats and 32-bit float, 16-bit,
  24-bit (packed or in 32) and 32-bit integer, clamped, NaN as silence, a
  round trip within half a step.
- Tests play silence only and never open a device named UMC or Behringer;
  the exclusive-mode test is run by hand. `examples/tone.rs` plays a quiet
  sine through a named device, for trying MARÉ by hand;
  [docs/HARDWARE.md](docs/HARDWARE.md) records what was tried.

### Doorbells on Linux

- A waiting thread sleeps on the doorbell's counter with a futex instead
  of polling it; ringing wakes it with `FUTEX_WAKE`. With a hundred
  clients under WSL, the hub's longest wait was 1.04 ms and no client was
  late. macOS still polls.

### Real-time priority

- `mare_sys::promote_current_thread` registers a thread with MMCSS as
  "Pro Audio" on Windows. The hub's cycle thread and `Client::run` use it.
  With a hundred client threads, the hub's longest wait fell from 21.8 ms
  to between 1.3 and 4.1 ms, on a budget of 3.7 ms.

### Protocol (`mare-protocol`)

- `include/mare.h`: the layout in C (C11 or C++11), with the structures,
  the constants, a layout computation and static checks of every size and
  offset. A test compiles and runs a C program with it and compares its
  layout with the Rust one over 360 configurations, and compiles it as C++.

- The shared-memory layout, version 0.1, described byte by byte in
  [docs/LAYOUT.md](docs/LAYOUT.md): the hub's registry (header, control
  block, one 64-byte slot per client), the client segment (header, control
  block, audio channels, two rings of timestamped UMP events), C layout,
  offsets instead of pointers, 64-byte aligned blocks, little-endian. Every
  offset follows from five numbers, and a reader refuses a header whose
  offsets differ from the ones it computes. No dependencies, no standard
  library.
- Single-producer single-consumer rings of `UmpEvent` (a frame time and four
  words) that report corruption instead of reading out of bounds, and the
  UMP packet length of each message type.
- Tests read and write segments through the documented offsets alone, as a
  program in another language would, and check the structures against them.

### System (`mare-sys`)

- `promote_current_thread` asks for `SCHED_FIFO` on Linux (within the
  user's `RLIMIT_RTPRIO`; a refusal is reported, not an error) and for a
  Mach time-constraint policy on macOS.
- Doorbells on macOS 14.4 and later sleep on the counter with
  `os_sync_wait_on_address`, shared between processes; older macOS polls.
- `Waiter::watching`: on Windows a wait also ends when a given process
  does. A client's waits watch its hub, so it learns of the hub's death in
  a few milliseconds instead of at its next 100 ms timeout.

- On Linux a `ProcessWatch` holds a `pidfd`, so a client that ends is
  seen exactly, reaped by its parent or not, and a reused process id
  cannot hide it. `ProcessWatch::is_exact` says whether a watch sees the
  end exactly (Windows and Linux) or by checking the id (elsewhere).

- On Windows every mapping and event is created with a protected access
  list that allows the current user and nobody else, instead of the token's
  default list (which also allows SYSTEM and administrators).
  `SharedMemory::is_private_to_user` reads it back.

- Named shared memory (file mappings on Windows, `shm_open` elsewhere),
  doorbells (a counter in shared memory, plus a named auto-reset event on
  Windows), a high-resolution waitable timer for the hub's sub-millisecond
  waits, and a watch that tells when a process has ended.

### Client (`mare-client`)

- `Client::connect` takes a slot in a hub's registry, creates the client's
  segment and doorbell, and waits to be adopted. `try_cycle`, `wait_cycle`
  and `run` run the client's cycle without allocating: read inputs and
  events, write outputs and events, mark the cycle done, ring the hub.

### Hub (`mare-hub`)

- A client sets a ready flag (client flags bit 1) when its audio thread
  first waits for a cycle, and the hub wakes, and counts as late, only
  ready clients: a client adopted while still connecting no longer starts
  with missed cycles.

- The `Engine` runs one cycle a period: it wakes each idle client with a
  deadline (70 % of the period by default), mixes the outputs of the clients
  that finish in time, counts the late ones, and does not wake or touch a
  client still on an earlier cycle. It never allocates, frees or locks.
- The `Housekeeper` adopts clients after validating their segments, refuses
  those it cannot trust with a reason, and frees those that leave or whose
  process ends, once the engine has let go of them.
- `mare_hub::start` runs both on their own threads; `mare_hub::create`
  returns them for a test to drive.
- A fake device and a fake clock make the cycle deterministic in tests.
- Tested: forty clients mixed, a late client losing only its own output, a
  busy client left alone, a hung client never holding the hub past its
  budget, slots freed and reused, events both ways, hostile segments
  refused, a hundred clients on their own threads, a client in another
  process killed in the middle of a cycle, and no heap calls on the cycle
  path of either side.
