# ADR 0001: Drift is corrected by the level of the queue between devices

**Status:** Accepted
**Date:** 2026-10-30
**Deciders:** Maintainers

## Context

Milestone 4 makes several devices behave as one. One device is the master
clock and paces the hub's cycles; every other device runs on its own crystal,
which differs from the master's by parts per million (a hundred is common,
several hundred possible) and wanders with temperature. Audio crossing from
one clock to the other must be resampled at the ratio of the two clocks, or a
queue between them grows or empties without end and eventually clicks.

That ratio is not known; it has to be estimated, while audio flows, and it
changes slowly. Two families of estimate exist:

- **from timestamps**: compare how many frames each device reports against
  a common clock (WASAPI's device and QPC positions, CoreAudio's timestamps)
  and fit a rate;
- **from the queue**: each device's audio goes through a queue; if the queue
  between them fills, the consumer is slower than the producer, so the
  resampler should consume a little faster, and the other way round.

The capture work of milestone 3 showed how unreliable device-side counts
are: in shared mode the Realtek microphone's positions count 44.1 kHz frames
behind a 48 kHz stream, and in exclusive mode the system hands every packet
over twice. Packets also arrive in bursts of a buffer at a time, so any
instantaneous measure is noisy by a whole packet.

## Decision

1. A device that is not the master is joined to the cycle through a queue,
   and an **adaptive resampler** between the queue and the hub, whose ratio
   is set every cycle by a **controller on the queue's level**: a
   proportional-integral loop that drives the level to a target, after a
   low-pass filter that removes the packet-size jumps. The integral term
   converges to the true ratio of the clocks; the proportional term keeps the
   level, and so the latency, bounded.
2. The ratio is kept within ±1000 ppm of nominal. A device further off than
   that is not drifting but running at another rate, which is configuration,
   not correction.
3. The resampler is a polyphase windowed-sinc filter, interpolating linearly
   between phases, so that any ratio near 1 is served without allocating and
   without changing its delay. Its latency is fixed and reported.
4. The level is measured finer than a packet: the queue, plus what the far
   device has recorded and not yet delivered, estimated from the time since
   its last packet arrived. The queue alone moves a whole packet at a time,
   and how often it moves depends on the drift (at 1 ppm and 128-frame
   packets, once every 44 minutes), so no filter can remove that staircase:
   the first simulation, fed the queue alone, locked into a swing of
   hundreds of ppm and an estimate 60 ppm off after two hours. Fed the finer
   level, it converged to within 0.2 ppm, and to within 3 ppm with ±30
   frames of timing error.
5. Timestamps are not used for the estimate itself, only for that part of a
   packet. They may later be used to *check* the estimate (and to report each
   device's measured rate), but the loop does not depend on a device
   reporting its own position truthfully.

## Options considered

### Option A: rate from timestamps

| Dimension | Assessment |
|---|---|
| Accuracy | High where timestamps are good |
| Robustness | Depends on every driver's positions being in the stream's frames and monotonic, which the Realtek already breaks |
| Latency control | None by itself: a small rate error still walks the queue away |

### Option B: controller on the queue's level (chosen)

| Dimension | Assessment |
|---|---|
| Accuracy | Converges to the true ratio; jitter set by the loop's bandwidth |
| Robustness | Needs nothing from the device but its audio |
| Latency control | Built in: the level is what is controlled |

**Cons:** it reacts in seconds, not instantly, so a device that starts far
from the target level takes a few seconds to settle; its bandwidth is a
trade-off between settling time and ratio jitter; and it needs the level to a
fraction of a packet, which takes the arrival time of each packet.

### Option C: drop or repeat samples, no resampling

**Cons:** every correction is a discontinuity, audible as a click at a rate
of the drift (a hundred ppm at 48 kHz is one every 0.2 s).

## Consequences

- `mare-drift` holds the resampler and the controller, with no dependencies
  and no allocation after construction, so the hub's cycle can run them.
- Tests drive the controller over hours of simulated time against fake
  clocks between −300 and +500 ppm, with packet-sized jitter, and require the
  queue never to empty or overflow and the ratio to converge; and run audio
  through the closed loop, requiring no discontinuity above what the signal
  itself has.
- The latency a resampled device adds is the queue's target level plus the
  resampler's delay, and both are reported.

## Action items

1. [x] The resampler and the controller, tested: the resampler's error is
   −91 to −123 dB RMS against exact sines at fixed and swept ratios; the
   controller holds the queue and estimates the drift over two simulated
   hours at −300 to +500 ppm; a sine crosses the closed loop at −102 dB RMS
2. [x] An aggregate device: a master and resampled others, on fake devices.
   Each follower runs on its own thread and is joined through a lock-free
   link that stamps each packet's arrival; on fake devices 300 ppm fast and
   500 ppm slow a sine crosses both ways at −103.5 dB RMS, never starved
3. [ ] Hot-plug: a device joining and leaving while the hub runs
4. [ ] Report each device's estimated ratio and the latency it adds (the
   aggregate gives both, per follower and direction; nothing shows them to
   a user yet)
5. [ ] Followers on real devices (WASAPI endpoints), tried by hand
