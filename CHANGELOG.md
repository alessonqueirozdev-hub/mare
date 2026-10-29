# Changelog

What changed in MARÉ, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break the protocol and the API.

## Unreleased

### Protocol (`mare-protocol`)

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
