# MARÉ on real hardware

What has been tried on real devices, and what came of it. Automated tests
play silence only and never open the Behringer UMC404HD; sound through real
hardware is tried by hand, with the person at the machine, and each session
is written down here, newest first.

To try it: `mare devices` lists the devices without opening any; `mare
run --device <part of a name>` keeps a hub on one (add `--exclusive` for
exclusive mode) and prints what the device reported; and, beside it,
`cargo run --release -p mare-client --example sine` plays a quiet sine
through the hub (−30 dBFS unless `--level` says otherwise, never above −6).
`cargo run -p mare-wasapi --example tone -- --device <part of a name>` does
both in one program.

## Machines

| Machine | Processor | System | Outputs |
|---|---|---|---|
| Development laptop | Intel Core i3-1215U (6 cores, 8 threads) | Windows 11 Pro 10.0.26200 | Realtek(R) Audio (built in); Behringer UMC404HD (USB, not connected on 2026-10-29 or 2026-11-01) |

## Sessions

### 2026-11-01: by hand, the `mare` program on the Realtek output

With the person at the machine listening, through the laptop's own
speakers, at a low volume; each step started once they said they were
ready. Release builds of `mare` and the `sine` example at commit 088f354,
Rust 1.99.0, each in a terminal tab of its own, the hub stopped with
Ctrl+C.

1. `mare devices`: one output, "Alto-falantes (2- Realtek(R) Audio)", and
   one input, its microphone, both the defaults; nothing opened.
2. `mare run --device Realtek`, shared mode: 32-bit float, 48 kHz, two
   outputs, period 480 frames (10 ms), buffer 1 056 frames; the driver
   reports a stream latency of 0. Beside it, `sine` at 440 Hz and −30 dBFS
   for 5 s: 501 cycles played, none late, its thread at real-time priority.
   The hub ran 2 436 cycles in 24 s with nothing late or busy, its longest
   wait 0.33 ms, and printed the client joining and leaving. Ctrl+C stopped
   it cleanly, with its summary. **Heard: clean.**
3. `mare run --device Realtek --exclusive`: 24-bit integer in 32-bit
   containers, 48 kHz, period and buffer 144 frames (3 ms), stream latency
   144 frames as the driver reports it. The same sine: 1 672 cycles played,
   none late. The hub ran 5 481 cycles in 16 s, nothing late or busy,
   longest wait 0.38 ms; Ctrl+C stopped it cleanly. **Heard: clean.**

What it shows: a hub kept by `mare run` on a real device, in both modes,
plays a client that connects to it by name from another process, with no
missed cycle, the client's start included (on 2026-10-29 the `tone`
example, hub and client in one process, missed 48 cycles while starting in
exclusive mode; here the hub had been running for seconds before the client
came). What it does not show: the UMC404HD, `--input` and `--follow` on
real devices, periods below 3 ms, and latency measured with a loopback
cable.

### 2026-10-29: by hand, a client that waits to be ready

Exclusive mode on the Realtek output, silence, run in a terminal the person
watched, after clients began setting a ready flag and the hub began waking
only ready clients (`tests/device.rs`, ignored test).

While the client started: 22 cycles, the client in 20 of them, never late,
never busy; before the change, three runs had shown it late once and busy
3 to 17 times there. Over the measured 333 cycles (0.97 s): nothing late,
nothing busy, no heap calls. The hub's longest wait for the client over the
whole run was 0.18 ms, against 2.2 to 2.7 ms before, all of which had come
while the client started.

### 2026-10-29: by hand, where the meter's losses came from

The person watching a terminal; output silent; nothing recorded kept.

1. The meter now prints a timeline, each quarter second: frames the input
   delivered, how many the system flagged silent, packets of all zeros,
   padding, dropping and the queue. Run for 15 s in shared mode, it showed
   every 480-frame packet losing about 39 frames: 57 989 frames left out as
   "repeated" and 1 421 of 1 488 cycles padded. 441 of 480 is 44 100 of
   48 000: the microphone runs at 44.1 kHz behind the 48 kHz engine, and in
   shared mode a packet's device position counts the device's frames, not
   the stream's. Placing packets by position, which exclusive mode needs
   (it hands packets over twice), read that as overlap. It is now used in
   exclusive mode only. The earlier meter runs' losses were this, not the
   input's start, as had been written below.
2. Shared duplex test again: 200 cycles, input 95 686 frames for 96 000
   out (the difference less than a packet), nothing padded or dropped.
3. The meter again, 15 s, the person speaking throughout: 1 493 cycles,
   all in time; the input delivered 716 778 frames in 1 484 packets of
   about 483 frames, and 716 640 (1 493 × 480) were handed on: nothing
   padded, dropped, repeated or skipped. The queue rises by 3 frames a
   cycle and falls by a packet when a cycle gets none, between 465 and 980
   frames (10 to 20 ms). 3 discontinuities were flagged, as in every run,
   probably at the start, which the meter does not yet time. The first
   seconds of exact zeros seen in two earlier runs did not appear in this
   run or the one before.

### 2026-10-29: by hand, duplex with the Realtek microphone

Output "Alto-falantes (2- Realtek(R) Audio)" with input "Microfone (2-
Realtek(R) Audio)", the person at the machine, each step started on their
word. Outputs played silence throughout; nothing recorded was kept.

1. Shared mode (`tests/device.rs`, ignored test): both 32-bit float,
   48 kHz, two channels, period 480 frames. First run: over 2 s the queue
   padded 136 frames, because it handed on what had arrived at once and a
   packet that came a moment after the output's event left a cycle short.
   With one period of cushion added: 200 cycles, input 96 170 frames for
   96 000 out (the difference is less than one 480-frame packet, the edges
   of the measurement), nothing padded, nothing dropped, the client never
   late, no heap calls; 814 frames queued at the end, so the input lags the
   hub by 10 to 17 ms beyond the devices' own latency.
2. Exclusive mode: output 24-bit in 32, input 16-bit integer, 48 kHz,
   period 144 frames (3 ms). First run: the input delivered exactly twice
   the frames the output played. Checking each packet's device position
   showed why: the system handed every packet over a second time (666
   jumps back in 666 cycles), and the input counted the repeats as new
   recording. With packets placed by their position: 666 cycles, input
   95 904 frames for 95 904 out, 94 176 repeated frames left out, nothing
   padded or dropped, the client never late; 96 frames queued at the end,
   2 to 3 ms of lag.
   *Correction, the same day:* placing packets by position was then used in
   shared mode too, where it is wrong, and the meter runs of step 3 suffered
   from it; see the session above.
3. The meter (`examples/meter.rs`), shared mode, 10 s, the person speaking
   and clapping: the level read by the client rose from −120 to −78 dBFS
   (the first 3 s, mostly exact zeros) to −30 to −13 dBFS while they made
   sound. 929 cycles, every one in time. Over the whole run, start
   included, the input padded 35 520 frames, dropped 4 761 and reported 3
   discontinuities. These were put down here to the input's start; that was
   wrong (see the session above): almost all of them came from placing
   shared-mode packets by a position that counts other frames.
   That run printed its levels where only the assistant saw them, after it
   ended; it was run again for 15 s in a terminal the person watched. They
   saw the levels follow their voice, from about −120 dBFS (exact zeros for
   the first 3 s again) to −11 to −30 dBFS, all 1 501 cycles in time; over
   the run, start included, 57 887 frames padded, 1 707 dropped and 3
   discontinuities, the same pattern as before.

### 2026-10-29: by hand, Realtek output of the development laptop

With the person at the machine listening, through the laptop's own
speakers, at a comfortable volume. Three steps, each started only once they
said they were ready. (The same three had been run once before, by mistake,
without anyone listening; those runs are not counted here.)

1. Exclusive mode, silence (`tests/device.rs`, ignored test). The device
   took 24-bit integer in 32-bit containers at 48 kHz, two channels, with a
   period of 144 frames (3 ms); the buffer is one period and the driver
   reports a stream latency of 144 frames. Over the measured 333 cycles
   (0.99 s): nothing late, nothing busy, no heap calls. While the client was
   starting, before the measurement: late once and busy 15 times, the
   longest wait (2.45 ms) among them.
2. A 440 Hz sine at −30 dBFS for 5 s, shared mode (`examples/tone.rs`):
   32-bit float, period 480 frames (10 ms, the driver's minimum), buffer
   1 056 frames. 478 cycles, every one finished in time, longest wait
   0.25 ms. **Heard: clean, no clicks or gaps.**
3. The same sine, exclusive mode: period 144 frames (3 ms). 1 596 cycles;
   the client finished 1 548 in time, was late once and busy 47 times, and
   the longest wait was 2.6 ms. **Heard: clean from start to end.** The 48
   missed cycles (about 144 ms) went unheard, which fits them all falling
   while the client started, before the tone began, as in step 1; the
   example did not yet tell starting apart from playing, and now does.

What it shows: the hub drives a real device in both modes, a client's
sound reaches the speakers through shared memory, and at 3 ms periods the
client keeps up once started. What it does not show: periods below 3 ms
(this output offers none), sound through the UMC404HD, duplex, and anything
measured with a loopback cable.

### 2026-10-29: automated silence, development laptop

Not by hand: `crates/mare-wasapi/tests/device.rs`, which the test suite
runs.

- Realtek(R) Audio, shared mode: the mixer's format, 32-bit float, 48 kHz,
  two channels. `IAudioClient3` offered no period shorter than 480 frames
  (10 ms), the driver's minimum, so the small periods shared mode can give
  are not available on this output. Buffer 1 056 frames. The driver reports
  a stream latency of 0.
- The hub, with one client writing silence: 100 cycles, 48 000 frames, in
  1.001 s; the client finished every cycle in time; the hub's longest wait
  for it was 0.12 ms; no heap calls on the cycle path.
- Exclusive mode not tried yet: it takes the device from every other
  program, so its test is run by hand.

Next, by hand: the tone through the Realtek output in shared and exclusive
mode, then through the UMC404HD in both modes, with the periods each offers
and whether anything clicks.
