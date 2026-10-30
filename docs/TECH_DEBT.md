# Technical debt

What the code owes, so that shortcuts are taken on purpose and paid back in
order. Update it when an item is paid back or a new one is taken on.

Last audit: 2026-10-29, covering every crate, during milestone 3.

## Scoring

Each item is scored from 1 to 5 on three axes, as in Manguebyte:

- **Impact**: how much it hurts people using or extending MARÉ.
- **Risk**: how bad it is if it is never fixed.
- **Effort**: how hard the fix is.

`Priority = (Impact + Risk) × (6 − Effort)`. Higher goes first.

## Open items

| # | Item | Type | Impact | Risk | Effort | Priority |
|---|---|---|---|---|---|---|
| 1 | Real-time priority is Windows-only: `promote_current_thread` registers a thread with MMCSS ("Pro Audio", high), which the hub's cycle thread and `Client::run` use, but changes nothing on Linux or macOS (`SCHED_FIFO` through RTKit, and Mach time-constraint threads, are the plan). On Windows it cut the hub's longest wait among a hundred client threads from 21.8 ms to 1.3 to 4.1 ms on a 3.7 ms budget; a CI runner without MMCSS still needs the real-time tests' loose bound | Code | 4 | 3 | 2 | 28 |
| 2 | Doorbells on macOS (and other Unix systems but Linux) poll their counter (64 spins, 32 yields, then sleeps of up to 50 µs), which adds up to about 60 µs per wake and keeps a waiting thread busy. Linux sleeps on the counter with a futex (a hundred clients under WSL: the hub's longest wait 1.04 ms, none late); a semaphore or `os_sync_wait_on_address` is the plan for macOS. `ring` makes a `FUTEX_WAKE` call even when nobody waits | Code | 2 | 2 | 2 | 16 |
| 4 | The hub reads and writes a client's audio through plain slices of shared memory. A client that writes its outputs after marking a cycle done races with the hub's read: the values may be garbage, and bounds are never at risk, but in Rust's memory model it is a data race. Copying with volatile reads, or through `AtomicU32`, would make a hostile client's effect defined | Code | 1 | 3 | 2 | 16 |
| 7 | The housekeeper looks at the registry every 2 ms instead of being woken, because the hub's only doorbell is the cycle thread's (two waiters on one auto-reset event would steal each other's wakes). Connecting costs up to 2 ms more than it needs, and an idle hub still wakes 500 times a second. A second doorbell for the housekeeper is the fix | Code | 1 | 1 | 1 | 10 |
| 8 | Events clients send are read and counted, not routed: there is no MIDI device backend and no event graph yet (milestone 5). The hub can put events into a client's ring (`Engine::send_event`), and both directions are tested | Architecture | 2 | 1 | 3 | 9 |
| 9 | The graph is fixed: channel *n* of every client reads channel *n* of the device and is summed into channel *n* of the device's output. There are no connections, no ordering between clients, and clients are not run in parallel on purpose (they run in parallel only because each has its own thread) | Architecture | 3 | 1 | 4 | 8 |
| 11 | A client that dies between taking a slot (compare-and-swap to `CLAIMED`) and storing its process id, two consecutive instructions, leaves the slot claimed until the hub restarts | Code | 1 | 1 | 2 | 8 |
| 12 | There is a C header (`include/mare.h`), checked against the Rust layout by compiling and running C (on Linux, where a compiler is on the path; Windows here has none), but no C client library: connecting, the doorbells and the cycle are Rust-only, so another language must reimplement them from DESIGN.md and LAYOUT.md | Architecture | 3 | 2 | 3 | 15 |
| 13 | The real-time tests (`threads.rs`, `process.rs`) can only bound what holds on a loaded machine, so their bounds are loose: a client that keeps up must be heard in half the cycles, and the hub's longest wait must stay within 100 ms of the budget. The exact accounting is proved on a fake clock (`cycles.rs`) | Test | 1 | 2 | 3 | 9 |
| 14 | A client learns that its hub has gone only when one of its waits times out (every 100 ms in `Client::run`). Waiting on the hub's process handle together with the doorbell would make it immediate on Windows | Code | 1 | 1 | 2 | 8 |
| 15 | Duplex pairs one input with one output in the same mode at the same sample rate, and assumes the two share a clock (true of one interface's endpoints); the capture queue pads and drops, counted, if they do not. Inputs at other rates, or on another device's clock, wait for milestone 4's drift correction. The input lags the hub by one period of cushion plus up to a packet: 2 to 3 ms in exclusive mode and 10 to 17 ms in shared mode on the Realtek | Architecture | 3 | 2 | 3 | 15 |
| 16 |
| 17 | WASAPI has run on one device only, the laptop's Realtek: output in exclusive mode (3 ms) and shared mode (10 ms, its driver's shortest) heard clean by hand, and duplex with its microphone in both modes (docs/HARDWARE.md). Small shared periods, other drivers and the UMC404HD are untried | Test | 3 | 3 | 1 | 30 |
| 18 |
| 19 | Latency is said only as WASAPI reports it (period, buffer, the driver's stream latency, which the Realtek driver gives as 0 in shared mode); nothing yet measures a round trip through a loopback cable, which DESIGN.md promises | Test | 3 | 2 | 3 | 15 |
| 20 | Exclusive mode tries formats at the mixer's channel count and sample rate (or the one asked for) only; a device whose exclusive formats have other channel counts is refused rather than adapted | Code | 2 | 1 | 2 | 12 |
| 22 | Two earlier meter runs on the Realtek microphone (shared) read exact zeros for their first 3 s; the next two did not. What causes it (the microphone waking, the system's effects, something else) is unknown, and the meter does not time the 3 discontinuities every run reports | Test | 1 | 2 | 1 | 15 |

## Paid back

| # | Item | How |
|---|---|---|
| 21 | A client joining at 3 ms periods lost its first 10 to 150 ms (late once, then busy 3 to 47 times): the hub woke it while it was still connecting | Clients set a ready flag when they first wait, and the hub wakes only ready clients. On the Realtek in exclusive mode the start then had nothing late or busy, and the hub's longest wait fell from 2.2 to 2.7 ms to 0.18 ms (2026-10-29) |
| 6 | Objects were protected only by where they lived (the session's `Local\` namespace, the token's default access list, which also lets in SYSTEM and administrators) | Every mapping and event MARÉ creates on Windows now has a protected access list with one entry, allowing the current user; a test reads it back, and another shows a mapping with the default list read as not private (2026-10-29) |
| 10 | Checking whether every client had finished walked every awaited client after each wake: up to *n²* loads a cycle | The engine keeps the clients it woke in order and how many have finished, so each is checked until it finishes and not again; with a hundred clients one run's longest wait was 0.22 ms, against 1.3 to 4.1 ms before (2026-10-30) |
| 5 | On Unix a dead client was found with `kill(pid, 0)`, fooled by a zombie or a reused id | On Linux 5.3 and later the watch holds a `pidfd`, readable when the process ends, reaped or not; a test sees an unreaped child end. Other Unix systems (macOS) still check the id, and `ProcessWatch::is_exact` says so (2026-10-30) |
| 3 | CI existed only as a file, and macOS, Arm Linux and the minimum Rust version had never built MARÉ | The repository is public (github.com/alessonqueirozdev-hub/mare) and CI runs on every push: tests on Ubuntu x86 and Arm, Windows and macOS, format, lint and documentation on three systems, and Rust 1.85. Its first run failed only on documentation warnings, fixed in the next commit, which ran green (2026-10-30) |
