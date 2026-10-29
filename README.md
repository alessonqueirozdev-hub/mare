<p align="center">
  <img src="assets/mare-logo.png" alt="MARÉ logo: a wave of sand and steel with circuits inside, in the Manguebyte ecosystem's orange, ringed by a laptop, a phone and a tablet showing sound waves" width="360">
</p>

# MARÉ

**Multi-device Audio Realtime Exchange**: an open protocol, and a hub that
implements it, for moving audio and MIDI between applications and audio
devices with low latency.

*Maré* is Portuguese for the tide. The mangrove lives by it, filling and
emptying in cycles, as audio buffers do.

MARÉ keeps what makes low-latency audio fast (applications write straight
into the buffers the device plays, on the device's schedule) and adds what
ASIO never had:

- **many applications at once** on the same device, each with low latency;
- **many devices as one**, with their clocks' drift corrected;
- **hot-plug**: devices come and go while the session runs;
- **MIDI 2.0 on the same clock** as the audio, to the sample;
- **latency said exactly**, part by part, and measured;
- **open**: MIT or Apache-2.0, a C layout any language can implement.

It is part of the ecosystem of the
[Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte),
from Recife: every program built on Manguebyte will speak MARÉ.

> **Status: milestone 2.** The shared-memory layout, the client library and
> a hub tested against fake devices are built and tested on Windows and
> Linux. Milestone 3 is under way: a WASAPI output device, which has played
> silence through one real device ([docs/HARDWARE.md](docs/HARDWARE.md)). [docs/DESIGN.md](docs/DESIGN.md)
> is the specification, including what MARÉ is not (not a kernel driver, and
> not lower latency than the hardware allows);
> [docs/LAYOUT.md](docs/LAYOUT.md) is the shared memory byte by byte, and
> [docs/TECH_DEBT.md](docs/TECH_DEBT.md) what the code still owes.

## The pieces

| Crate | What it is |
|---|---|
| `mare-protocol` | The shared-memory layout: no dependencies, no standard library |
| `mare-sys` | Named shared memory, doorbells, process watches, per system |
| `mare-client` | Connect to a hub and run a cycle each period |
| `mare-hub` | The hub: the cycle engine, the housekeeper, a fake device |
| `mare-wasapi` | A WASAPI output device, exclusive or shared (Windows) |

```bash
cargo test --workspace
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
