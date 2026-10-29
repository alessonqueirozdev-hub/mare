# Technical debt

What the code owes, so that shortcuts are taken on purpose and paid back in
order. Update it when an item is paid back or a new one is taken on.

Last audit: 2026-10-29, covering `mare-protocol`, `mare-sys`,
`mare-client` and `mare-hub` at the end of milestone 2.

## Scoring

Each item is scored from 1 to 5 on three axes, as in Manguebyte:

- **Impact**: how much it hurts people using or extending MARÉ.
- **Risk**: how bad it is if it is never fixed.
- **Effort**: how hard the fix is.

`Priority = (Impact + Risk) × (6 − Effort)`. Higher goes first.

## Open items

| # | Item | Type | Impact | Risk | Effort | Priority |
|---|---|---|---|---|---|---|
| 1 | The cycle thread runs at normal priority: it is not registered with MMCSS ("Pro Audio") on Windows nor given a real-time priority elsewhere, so any runnable thread can delay it by a scheduler quantum. With a hundred client threads waking at once on eight logical cores, one wait of 21.8 ms was measured on a 3.7 ms budget (`tests/threads.rs`, whose bound is 100 ms because of it). Client threads have the same problem, and the client library offers no way to raise them | Code | 5 | 4 | 2 | 36 |
| 2 | Doorbells on Linux and macOS poll their counter (64 spins, 32 yields, then sleeps of up to 50 µs) because there is no wake object yet. It works and is tested on Linux, but adds up to about 60 µs of latency per wake and keeps a waiting thread busy. A futex on the counter (Linux) and a semaphore or `os_sync_wait_on_address` (macOS) are the plan; the counter already lives in shared memory for that reason | Code | 3 | 3 | 2 | 24 |
| 3 | CI exists only as a file: there is no remote repository, so `.github/workflows/ci.yml` has never run. Windows and Linux (Ubuntu under WSL, x86-64) are tested by hand on the development laptop; macOS and Arm Linux have never compiled MARÉ, and nor has the minimum Rust version (1.85) | Test | 3 | 4 | 1 | 35 |
| 4 | The hub reads and writes a client's audio through plain slices of shared memory. A client that writes its outputs after marking a cycle done races with the hub's read: the values may be garbage, and bounds are never at risk, but in Rust's memory model it is a data race. Copying with volatile reads, or through `AtomicU32`, would make a hostile client's effect defined | Code | 1 | 3 | 2 | 16 |
| 5 | On Unix a dead client is found with `kill(pid, 0)`, which counts a process that has ended but not been reaped as alive and is fooled by a reused id. `pidfd_open` (Linux) and `EVFILT_PROC` (macOS) would be exact | Code | 2 | 2 | 2 | 16 |
| 6 | Objects are protected by where they live, not by an explicit access list: on Windows they are in the session's `Local\` namespace with the default access list of the creator's token; elsewhere shared memory is created with mode `0600`. DESIGN.md asks for an access list for the user alone on Windows | Security | 2 | 3 | 2 | 20 |
| 7 | The housekeeper looks at the registry every 2 ms instead of being woken, because the hub's only doorbell is the cycle thread's (two waiters on one auto-reset event would steal each other's wakes). Connecting costs up to 2 ms more than it needs, and an idle hub still wakes 500 times a second. A second doorbell for the housekeeper is the fix | Code | 1 | 1 | 1 | 10 |
| 8 | Events clients send are read and counted, not routed: there is no MIDI device backend and no event graph yet (milestone 5). The hub can put events into a client's ring (`Engine::send_event`), and both directions are tested | Architecture | 2 | 1 | 3 | 9 |
| 9 | The graph is fixed: channel *n* of every client reads channel *n* of the device and is summed into channel *n* of the device's output. There are no connections, no ordering between clients, and clients are not run in parallel on purpose (they run in parallel only because each has its own thread) | Architecture | 3 | 1 | 4 | 8 |
| 10 | Checking whether every client has finished walks every awaited client after each wake, so a cycle with *n* clients costs up to *n²* atomic loads. A hundred clients measure well; a count of clients still pending would make it linear | Code | 1 | 1 | 1 | 10 |
| 11 | A client that dies between taking a slot (compare-and-swap to `CLAIMED`) and storing its process id, two consecutive instructions, leaves the slot claimed until the hub restarts | Code | 1 | 1 | 2 | 8 |
| 12 | There is no C header (`mare.h`) or C client library, so "any language can implement either side" is a claim about the layout, checked in Rust against byte offsets but not yet from a second language | Architecture | 3 | 2 | 3 | 15 |
| 13 | The real-time tests (`threads.rs`, `process.rs`) can only bound what holds on a loaded machine, so their bounds are loose: a client that keeps up must be heard in half the cycles, and the hub's longest wait must stay within 100 ms of the budget. The exact accounting is proved on a fake clock (`cycles.rs`) | Test | 1 | 2 | 3 | 9 |
| 14 | A client learns that its hub has gone only when one of its waits times out (every 100 ms in `Client::run`). Waiting on the hub's process handle together with the doorbell would make it immediate on Windows | Code | 1 | 1 | 2 | 8 |
