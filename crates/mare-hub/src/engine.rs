//! The cycle thread.
//!
//! [`Engine::run_cycle`] runs one cycle: it waits for the device's next
//! period, reads the device's input, hands it to every client that is idle
//! and wakes it, waits for the clients until their deadline, mixes the
//! outputs of those that finished, and hands the mix to the device.
//!
//! The same cycle can be run in two halves, [`Engine::begin_cycle`] and
//! [`Engine::end_cycle`], so that a test on one thread can run some clients
//! in between and leave others late.
//!
//! Nothing here allocates, frees, or takes a lock. The only waits are the
//! device's and the deadline-bounded wait on the hub's doorbell; the only
//! other system calls are the signals that wake clients.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mare_protocol::ring::UmpEvent;
use mare_protocol::segment::client_flags;
use mare_sys::Waiter;

use crate::clock::Clock;
use crate::device::{AudioBlock, Device, DeviceError};
use crate::shared::{Counters, Shared, Stats, phase};

/// A client of the hub, as the cycle thread knows it: its slot, and the
/// slot's generation, so that a later client in the same slot is not taken
/// for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientId {
    /// The registry slot.
    pub slot: u32,
    /// The slot's generation when the client connected.
    pub generation: u32,
}

/// What happened in one cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CycleReport {
    /// The cycle's number, from 1.
    pub cycle: u64,
    /// The frame of the master clock at the cycle's first frame.
    pub frame_time: u64,
    /// Frames in the cycle.
    pub frames: usize,
    /// Clients woken.
    pub woken: u32,
    /// Clients that finished in time; their outputs are in the mix.
    pub on_time: u32,
    /// Clients woken that did not finish in time.
    pub late: u32,
    /// Clients not woken, because they were still on an earlier cycle.
    pub busy: u32,
    /// How long the hub waited for its clients.
    pub waited: Duration,
}

#[derive(Clone, Copy, Default)]
struct Local {
    /// The last cycle this client was woken for, or zero.
    woken: u64,
    /// Whether it was woken for the current cycle.
    awaited: bool,
}

/// The hub's cycle thread.
pub struct Engine<D: Device> {
    shared: Arc<Shared>,
    device: D,
    clock: Arc<dyn Clock>,
    waiter: Waiter,
    budget: Duration,
    stride: usize,
    inputs: Vec<f32>,
    outputs: Vec<f32>,
    local: Vec<Local>,
    /// The slots woken this cycle, in order, and how many of them, from the
    /// start, are known to have finished. Allocated once, for every slot.
    awaited: Vec<u32>,
    finished: usize,
    report: CycleReport,
    next_frame_time: u64,
    deadline: Duration,
    wait_started: Duration,
    in_cycle: bool,
}

impl<D: Device> Engine<D> {
    pub(crate) fn new(
        shared: Arc<Shared>,
        device: D,
        clock: Arc<dyn Clock>,
        budget: Duration,
    ) -> std::io::Result<Self> {
        let config = device.config();
        let stride = config.max_frames as usize;
        let slots = shared.slots.len();
        Ok(Self {
            shared,
            device,
            clock,
            waiter: Waiter::precise()?,
            budget,
            stride,
            inputs: vec![0.0; config.input_channels as usize * stride],
            outputs: vec![0.0; config.output_channels as usize * stride],
            local: vec![Local::default(); slots],
            awaited: Vec::with_capacity(slots),
            finished: 0,
            report: CycleReport::default(),
            next_frame_time: 0,
            deadline: Duration::ZERO,
            wait_started: Duration::ZERO,
            in_cycle: false,
        })
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Tells every client the hub has stopped, and marks the registry so.
    /// The engine does this when dropped; a caller that keeps the engine
    /// after its last cycle calls it to let the clients go.
    pub fn shut_down(&self) {
        self.shared.shut_down();
    }

    /// The device.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// The device, to change.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// The hub's counters.
    pub fn stats(&self) -> Stats {
        self.shared.counters.snapshot()
    }

    /// How long clients have, from being woken, to finish a cycle.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// Runs one whole cycle.
    pub fn run_cycle(&mut self) -> Result<CycleReport, DeviceError> {
        self.begin_cycle()?;
        self.end_cycle()
    }

    /// The first half of a cycle: waits for the device's period, reads its
    /// input, and wakes every idle client.
    pub fn begin_cycle(&mut self) -> Result<(), DeviceError> {
        assert!(!self.in_cycle, "begin_cycle twice without end_cycle");
        let config = self.device.config();
        let frames = self.device.wait_period()?.min(self.stride);
        let cycle = self.report.cycle + 1;
        self.report = CycleReport {
            cycle,
            frame_time: self.next_frame_time,
            frames,
            ..CycleReport::default()
        };
        self.next_frame_time += frames as u64;
        self.shared
            .view
            .control()
            .cycle
            .store(cycle, Ordering::Release);

        let device_inputs = config.input_channels as usize;
        self.device.read(&mut AudioBlock::new(
            &mut self.inputs,
            device_inputs,
            self.stride,
            frames,
        ))?;

        let start = self.clock.now();
        self.deadline = start + self.budget;
        let budget_ns = self.budget.as_nanos() as u64;
        let Self {
            shared,
            local,
            report,
            inputs,
            stride,
            awaited,
            finished,
            ..
        } = self;
        // Within capacity: there is room for every slot.
        awaited.clear();
        *finished = 0;
        for (index, (slot, local)) in shared.slots.iter().zip(local.iter_mut()).enumerate() {
            local.awaited = false;
            let current = slot.phase.load(Ordering::Acquire);
            if current == phase::RETIRING {
                *local = Local::default();
                slot.phase.store(phase::RETIRED, Ordering::Release);
                // The housekeeper frees it as soon as it knows; one signal,
                // only when a client goes.
                shared.keeper_bell.ring();
                continue;
            }
            if current != phase::LIVE {
                *local = Local::default();
                continue;
            }
            // SAFETY: this is the cycle thread and the phase is LIVE.
            let Some(client) = (unsafe { slot.engine() }) else {
                continue;
            };
            let control = client.view.control();
            if control.client_flags.load(Ordering::Acquire) & client_flags::READY == 0 {
                // Adopted, but its audio thread is not waiting yet: a cycle
                // it could not have run is not counted against it.
                continue;
            }
            if local.woken != 0 && control.done_cycle.load(Ordering::Acquire) != local.woken {
                // Still on an earlier cycle: its buffers are its own until it
                // finishes, so it is neither written nor woken.
                report.busy += 1;
                Counters::add(&shared.counters.busy, 1);
                if let Some(registry_slot) = shared.view.slot(index as u32) {
                    Counters::add(&registry_slot.late_cycles, 1);
                }
                continue;
            }
            let layout = client.view.layout().config();
            for channel in 0..layout.input_channels {
                let Some(dst) = client.view.input(channel) else {
                    break;
                };
                // SAFETY: the channel holds `max_frames >= frames` samples,
                // and the client is idle, so nobody else touches it.
                let dst = unsafe { std::slice::from_raw_parts_mut(dst.as_ptr(), frames) };
                if (channel as usize) < device_inputs {
                    let start = channel as usize * *stride;
                    dst.copy_from_slice(&inputs[start..start + frames]);
                } else {
                    dst.fill(0.0);
                }
            }
            control.frames.store(frames as u32, Ordering::Relaxed);
            control
                .frame_time
                .store(report.frame_time, Ordering::Relaxed);
            control.budget_ns.store(budget_ns, Ordering::Relaxed);
            control.cycle.store(cycle, Ordering::Release);
            client.go.ring();
            local.woken = cycle;
            local.awaited = true;
            awaited.push(index as u32);
            report.woken += 1;
        }
        self.in_cycle = true;
        self.wait_started = self.clock.now();
        Ok(())
    }

    /// Whether every client woken this cycle has finished it. Clients are
    /// checked in the order they were woken, and one known to have finished
    /// is not checked again, so a cycle costs one check per client plus one
    /// per wake, however many wakes there are.
    fn all_done(&mut self) -> bool {
        let cycle = self.report.cycle;
        while let Some(&index) = self.awaited.get(self.finished) {
            let slot = &self.shared.slots[index as usize];
            // SAFETY: awaited clients are LIVE or RETIRING, owned by this
            // thread until it stores RETIRED at a later cycle.
            let done = unsafe { slot.engine() }.is_none_or(|client| {
                client.view.control().done_cycle.load(Ordering::Acquire) == cycle
            });
            if !done {
                return false;
            }
            self.finished += 1;
        }
        true
    }

    /// The second half of a cycle: waits for the clients until their
    /// deadline, mixes the outputs of those that finished, and hands the mix
    /// to the device.
    pub fn end_cycle(&mut self) -> Result<CycleReport, DeviceError> {
        assert!(self.in_cycle, "end_cycle without begin_cycle");
        self.in_cycle = false;
        loop {
            let seen = self.shared.hub_bell.seen();
            if self.all_done() || self.clock.now() >= self.deadline {
                break;
            }
            self.clock
                .wait_bell(&self.shared.hub_bell, &mut self.waiter, seen, self.deadline);
        }
        let waited = self.clock.now().saturating_sub(self.wait_started);
        self.report.waited = waited;
        self.shared
            .counters
            .longest_wait_ns
            .fetch_max(waited.as_nanos() as u64, Ordering::Relaxed);

        let config = self.device.config();
        let device_outputs = config.output_channels as usize;
        let frames = self.report.frames;
        let cycle = self.report.cycle;
        for channel in 0..device_outputs {
            let start = channel * self.stride;
            self.outputs[start..start + frames].fill(0.0);
        }
        let Self {
            shared,
            local,
            report,
            outputs,
            stride,
            ..
        } = self;
        for (index, (slot, local)) in shared.slots.iter().zip(local.iter()).enumerate() {
            if !local.awaited {
                continue;
            }
            // SAFETY: awaited clients are LIVE, owned by this thread.
            let Some(client) = (unsafe { slot.engine() }) else {
                continue;
            };
            let registry_slot = shared.view.slot(index as u32);
            if client.view.control().done_cycle.load(Ordering::Acquire) != cycle {
                report.late += 1;
                Counters::add(&shared.counters.late, 1);
                if let Some(registry_slot) = registry_slot {
                    Counters::add(&registry_slot.late_cycles, 1);
                }
                continue;
            }
            report.on_time += 1;
            Counters::add(&shared.counters.client_cycles, 1);
            if let Some(registry_slot) = registry_slot {
                Counters::add(&registry_slot.cycles, 1);
            }
            let client_outputs = client.view.layout().config().output_channels as usize;
            for channel in 0..client_outputs.min(device_outputs) {
                let Some(src) = client.view.output(channel as u32) else {
                    break;
                };
                // A well-behaved client does not touch its outputs once it
                // has marked the cycle done, but the hub does not trust it:
                // each sample is read with a volatile read, so that a client
                // still writing gives the mix wrong values and nothing worse.
                let start = channel * *stride;
                for (i, out) in outputs[start..start + frames].iter_mut().enumerate() {
                    // SAFETY: the channel holds `max_frames >= frames`
                    // samples, mapped while the client is LIVE.
                    *out += unsafe { src.as_ptr().add(i).read_volatile() };
                }
            }
            if let Some(ring) = client.events_out.as_mut() {
                let mut received = 0;
                loop {
                    match ring.pop() {
                        Ok(Some(_)) => received += 1,
                        Ok(None) => break,
                        Err(_) => {
                            ring.discard_all();
                            Counters::add(&shared.counters.events_corrupt, 1);
                            break;
                        }
                    }
                }
                Counters::add(&shared.counters.events_received, received);
            }
        }
        self.device.write(&AudioBlock::new(
            &mut self.outputs,
            device_outputs,
            self.stride,
            frames,
        ))?;
        Counters::add(&self.shared.counters.cycles, 1);
        Ok(self.report)
    }

    /// The clients the cycle thread runs now.
    pub fn clients(&self) -> impl Iterator<Item = ClientId> + '_ {
        self.shared
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                if slot.phase.load(Ordering::Acquire) != phase::LIVE {
                    return None;
                }
                // SAFETY: this is the cycle thread (it holds `&self`), and
                // the phase is LIVE.
                let client = unsafe { slot.engine() }?;
                Some(ClientId {
                    slot: index as u32,
                    generation: client.generation,
                })
            })
    }

    /// Puts `event` in the ring of events into `client`, for it to read in a
    /// later cycle. Hands it back if the client is gone, has no ring, or its
    /// ring is full.
    pub fn send_event(&mut self, client: ClientId, event: UmpEvent) -> Result<(), UmpEvent> {
        let Some(slot) = self.shared.slots.get(client.slot as usize) else {
            return Err(event);
        };
        if slot.phase.load(Ordering::Acquire) != phase::LIVE {
            return Err(event);
        }
        // SAFETY: this is the cycle thread and the phase is LIVE.
        match unsafe { slot.engine() } {
            Some(attached) if attached.generation == client.generation => {
                match attached.events_in.as_mut() {
                    Some(ring) => ring.push(event),
                    None => Err(event),
                }
            }
            _ => Err(event),
        }
    }
}

impl<D: Device> Drop for Engine<D> {
    fn drop(&mut self) {
        self.shared.shut_down();
    }
}
