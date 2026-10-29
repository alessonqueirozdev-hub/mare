# MARÉ on real hardware

What has been tried on real devices, and what came of it. Automated tests
play silence only and never open the Behringer UMC404HD; sound through real
hardware is tried by hand, with the person at the machine, and each session
is written down here, newest first.

To try it: `cargo run -p mare-wasapi --example list` lists the outputs
without opening any, and `cargo run -p mare-wasapi --example tone --
--device <part of a name>` plays a quiet sine (−30 dBFS unless `--level`
says otherwise, never above −6) and prints what the device and the hub
reported. Add `--exclusive` for exclusive mode.

## Machines

| Machine | Processor | System | Outputs |
|---|---|---|---|
| Development laptop | Intel Core i3-1215U (6 cores, 8 threads) | Windows 11 Pro 10.0.26200 | Realtek(R) Audio (built in); Behringer UMC404HD (USB, not connected on 2026-10-29) |

## Sessions

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
