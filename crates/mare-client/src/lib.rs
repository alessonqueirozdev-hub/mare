//! The client side of MARÉ.
//!
//! A [`Client`] connects to a running hub by name, creates its own segment
//! of shared memory (its audio buffers and event rings), and is adopted by
//! the hub. From then on, once a period, the hub writes the client's inputs
//! and rings its doorbell; the client runs, writes its outputs, marks the
//! cycle done and rings the hub's doorbell.
//!
//! ```no_run
//! use mare_client::{Client, ClientConfig};
//! use mare_sys::HubName;
//!
//! let mut client = Client::connect(&HubName::default_hub(), &ClientConfig::new("gain", 2, 2))?;
//! loop {
//!     let ran = client.wait_cycle(std::time::Duration::from_millis(100), |cycle| {
//!         for channel in 0..2 {
//!             let (input, output) = cycle.input_output(channel, channel).unwrap();
//!             for (out, x) in output.iter_mut().zip(input) {
//!                 *out = x * 0.5;
//!             }
//!         }
//!     });
//!     if ran.is_err() {
//!         break; // the hub stopped or went away
//!     }
//! }
//! # Ok::<(), mare_client::ConnectError>(())
//! ```
//!
//! [`Client::wait_cycle`], [`Client::try_cycle`] and [`Client::run`] are the
//! real-time path: they do not allocate or lock, and the only system calls
//! they make are the wait on the client's doorbell and the signal of the
//! hub's. A wait that times out checks that the hub is still running, which
//! is also a system call, but only then.

use std::fmt;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mare_protocol::LayoutError;
use mare_protocol::registry::{
    RegistryHeader, RegistryLayout, RegistrySlot, RegistryView, hub_state, reject_reason,
    slot_state,
};
use mare_protocol::ring::{RingConsumer, RingProducer, UmpEvent};
use mare_protocol::segment::{SegmentConfig, SegmentLayout, SegmentView, client_flags, hub_flags};
use mare_sys::{Doorbell, HubName, ProcessWatch, SharedMemory, Waiter, current_pid};

/// What a client asks of the hub when it connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// A name for people to read, at most 64 bytes of UTF-8.
    pub name: String,
    /// Audio channels into the client.
    pub input_channels: u32,
    /// Audio channels out of the client.
    pub output_channels: u32,
    /// Capacity of the ring of events into the client: zero, or a power of
    /// two.
    pub events_in_capacity: u32,
    /// Capacity of the ring of events out of the client: zero, or a power of
    /// two.
    pub events_out_capacity: u32,
    /// The latency the client adds, in frames.
    pub latency_frames: u32,
    /// How long to wait for the hub to answer.
    pub connect_timeout: Duration,
}

impl ClientConfig {
    /// A client with these channels, rings of 256 events each way, no
    /// latency of its own, and two seconds to connect.
    pub fn new(name: &str, input_channels: u32, output_channels: u32) -> Self {
        Self {
            name: name.to_owned(),
            input_channels,
            output_channels,
            events_in_capacity: 256,
            events_out_capacity: 256,
            latency_frames: 0,
            connect_timeout: Duration::from_secs(2),
        }
    }
}

/// How the hub runs, as its registry says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubInfo {
    /// Frames per second.
    pub sample_rate: u32,
    /// Frames in each cycle.
    pub period_frames: u32,
    /// The longest cycle, in frames.
    pub max_frames: u32,
    /// The hub's process id.
    pub hub_pid: u32,
}

/// Why a client could not connect.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectError {
    /// No hub of that name is running.
    NoHub(io::Error),
    /// The hub's registry exists, but the hub is starting or stopping.
    HubNotRunning,
    /// The registry does not follow this layout (another version?).
    BadRegistry(LayoutError),
    /// The configuration has no layout.
    Config(LayoutError),
    /// Every slot is taken.
    Full,
    /// The hub refused the client, for this [`reject_reason`].
    Rejected(u32),
    /// The hub did not answer in time.
    Timeout,
    /// The operating system refused something.
    Io(io::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHub(e) => write!(f, "no hub of that name is running: {e}"),
            Self::HubNotRunning => write!(f, "the hub is not running"),
            Self::BadRegistry(e) => write!(f, "the hub's registry is not understood: {e}"),
            Self::Config(e) => write!(f, "invalid client configuration: {e}"),
            Self::Full => write!(f, "the hub has no free slot"),
            Self::Rejected(reason) => write!(f, "the hub refused the client (reason {reason})"),
            Self::Timeout => write!(f, "the hub did not answer in time"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Why a client stopped running cycles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stopped {
    /// The hub is shutting down, or has dropped this client.
    HubStopped,
    /// The hub's process has ended.
    HubGone,
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HubStopped => write!(f, "the hub stopped"),
            Self::HubGone => write!(f, "the hub's process ended"),
        }
    }
}

impl std::error::Error for Stopped {}

/// A client connected to a hub.
pub struct Client {
    // Field order is drop order: the views and doorbells point into the
    // mappings, which go last.
    events_in: Option<RingConsumer>,
    events_out: Option<RingProducer>,
    go: Doorbell,
    hub_bell: Doorbell,
    keeper_bell: Doorbell,
    waiter: Waiter,
    hub_process: ProcessWatch,
    view: SegmentView,
    registry_view: RegistryView,
    slot: u32,
    generation: u32,
    last_cycle: u64,
    ready: bool,
    info: HubInfo,
    _segment: SharedMemory,
    _registry: SharedMemory,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .field("last_cycle", &self.last_cycle)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// Frees a claimed slot if connecting fails part way.
struct Claim<'a> {
    slot: &'a RegistrySlot,
    armed: bool,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Nothing of ours is published yet, so the slot can simply be
            // handed back.
            self.slot.client_pid.store(0, Ordering::Relaxed);
            self.slot.state.store(slot_state::FREE, Ordering::Release);
        }
    }
}

/// Moves `slot` from `from` to LEAVING, unless the hub has moved it on
/// already (a plain store could overwrite a new client's claim on a slot the
/// hub has freed).
fn give_up(slot: &RegistrySlot, from: u32) -> bool {
    slot.state
        .compare_exchange(
            from,
            slot_state::LEAVING,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_ok()
}

impl Client {
    /// Connects to the hub called `hub`.
    pub fn connect(hub: &HubName, config: &ClientConfig) -> Result<Self, ConnectError> {
        let registry = SharedMemory::open(&hub.registry()).map_err(ConnectError::NoHub)?;
        let mapped = registry.len() as u64;
        // SAFETY: the mapping lives in `registry`, which the client keeps.
        let header = unsafe { RegistryHeader::read_from(registry.base(), mapped) }
            .map_err(ConnectError::BadRegistry)?;
        // SAFETY: `read_from` checked the control block is mapped.
        let control = unsafe { RegistryView::control_at(registry.base()) };
        if control.hub_state.load(Ordering::Acquire) != hub_state::RUNNING {
            return Err(ConnectError::HubNotRunning);
        }
        // Read again now that RUNNING has published it.
        // SAFETY: as above.
        let header =
            unsafe { RegistryHeader::read_from(registry.base(), mapped) }.unwrap_or(header);
        let layout =
            RegistryLayout::validate(&header, mapped).map_err(ConnectError::BadRegistry)?;
        // SAFETY: validated against the mapped length; kept with the mapping.
        let registry_view = unsafe { RegistryView::from_raw(registry.base(), mapped, layout) }
            .map_err(ConnectError::BadRegistry)?;
        let info = HubInfo {
            sample_rate: header.sample_rate,
            period_frames: header.period_frames,
            max_frames: header.max_frames,
            hub_pid: header.hub_pid,
        };
        let segment_layout = SegmentLayout::new(SegmentConfig {
            input_channels: config.input_channels,
            output_channels: config.output_channels,
            max_frames: header.max_frames,
            events_in_capacity: config.events_in_capacity,
            events_out_capacity: config.events_out_capacity,
        })
        .map_err(ConnectError::Config)?;
        let segment_header = segment_layout
            .header(&config.name, config.latency_frames)
            .map_err(ConnectError::Config)?;
        let hub_process = ProcessWatch::open(header.hub_pid).map_err(ConnectError::NoHub)?;
        // A second watch of the hub, for the waiter, so that a wait ends the
        // moment the hub's process does.
        let waiter = Waiter::coarse()
            .watching(ProcessWatch::open(header.hub_pid).map_err(ConnectError::NoHub)?);

        // Take a free slot.
        let (slot, slot_ref) = (0..registry_view.slot_count())
            .find_map(|i| {
                let slot = registry_view.slot(i)?;
                slot.state
                    .compare_exchange(
                        slot_state::FREE,
                        slot_state::CLAIMED,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                    .then_some((i, slot))
            })
            .ok_or(ConnectError::Full)?;
        slot_ref.client_pid.store(current_pid(), Ordering::Release);
        let mut claim = Claim {
            slot: slot_ref,
            armed: true,
        };
        let generation = slot_ref.generation.load(Ordering::Acquire);

        // Make the segment and the doorbells.
        let size = segment_layout.size();
        let segment = SharedMemory::create(&hub.segment(slot, generation), size as usize)?;
        // SAFETY: the segment is mapped for at least `size` bytes, and is
        // kept with the view.
        let view =
            unsafe { SegmentView::from_raw(segment.base(), segment.len() as u64, segment_layout) }
                .map_err(ConnectError::Config)?;
        // SAFETY: nobody else can see the segment until REQUESTED.
        unsafe { view.write_header(&segment_header) };
        // SAFETY: the bell is in the segment, which outlives the doorbell.
        let go = unsafe {
            Doorbell::create(
                NonNull::from(&view.control().bell),
                &hub.client_wake(slot, generation),
            )
        }?;
        // SAFETY: the hub's bell is in the registry, which outlives the
        // doorbell.
        let hub_bell = unsafe {
            Doorbell::open(
                NonNull::from(&registry_view.control().hub_bell),
                &hub.hub_wake(),
            )
        }?;
        // SAFETY: the housekeeper's bell is in the registry too.
        let keeper_bell = unsafe {
            Doorbell::open(
                NonNull::from(&registry_view.control().keeper_bell),
                &hub.keeper_wake(),
            )
        }?;
        // SAFETY: this client is the only consumer of its input ring and the
        // only producer of its output ring.
        let (events_in, events_out) = unsafe {
            (
                view.events_in().map(|r| RingConsumer::new(r)),
                view.events_out().map(|r| RingProducer::new(r)),
            )
        };

        // Ask the hub to adopt it.
        slot_ref.segment_size.store(size, Ordering::Relaxed);
        if slot_ref
            .state
            .compare_exchange(
                slot_state::CLAIMED,
                slot_state::REQUESTED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // The hub freed the slot under us: it believed us dead.
            claim.armed = false;
            return Err(ConnectError::Timeout);
        }
        claim.armed = false;
        keeper_bell.ring();

        let start = Instant::now();
        loop {
            match slot_ref.state.load(Ordering::Acquire) {
                slot_state::ACCEPTED => break,
                slot_state::REJECTED => {
                    let reason = slot_ref.reject_reason.load(Ordering::Acquire);
                    give_up(slot_ref, slot_state::REJECTED);
                    keeper_bell.ring();
                    return Err(ConnectError::Rejected(reason));
                }
                slot_state::REQUESTED => {}
                _ => return Err(ConnectError::Rejected(reject_reason::NONE)),
            }
            if start.elapsed() >= config.connect_timeout {
                // The hub may be accepting at this very moment; LEAVING
                // tells it to undo that.
                if !give_up(slot_ref, slot_state::REQUESTED) {
                    give_up(slot_ref, slot_state::ACCEPTED);
                }
                keeper_bell.ring();
                return Err(ConnectError::Timeout);
            }
            if control.hub_state.load(Ordering::Acquire) != hub_state::RUNNING {
                give_up(slot_ref, slot_state::REQUESTED);
                return Err(ConnectError::HubNotRunning);
            }
            std::thread::sleep(Duration::from_micros(200));
        }

        Ok(Self {
            events_in,
            events_out,
            go,
            hub_bell,
            keeper_bell,
            waiter,
            hub_process,
            view,
            registry_view,
            slot,
            generation,
            last_cycle: 0,
            ready: false,
            info,
            _segment: segment,
            _registry: registry,
        })
    }

    /// How the hub runs.
    pub fn info(&self) -> HubInfo {
        self.info
    }

    /// The registry slot this client has.
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// The last cycle this client finished, or zero.
    pub fn last_cycle(&self) -> u64 {
        self.last_cycle
    }

    /// Cycles the hub says this client missed.
    pub fn late_cycles(&self) -> u64 {
        self.slot_ref().late_cycles.load(Ordering::Relaxed)
    }

    fn slot_ref(&self) -> &RegistrySlot {
        self.registry_view
            .slot(self.slot)
            .expect("the slot was found in this registry")
    }

    fn check_stop(&self) -> Result<(), Stopped> {
        let flags = self.view.control().hub_flags.load(Ordering::Acquire);
        if flags & hub_flags::STOP != 0 {
            return Err(Stopped::HubStopped);
        }
        Ok(())
    }

    fn check_hub(&self) -> Result<(), Stopped> {
        self.check_stop()?;
        if self
            .registry_view
            .control()
            .hub_state
            .load(Ordering::Acquire)
            != hub_state::RUNNING
        {
            return Err(Stopped::HubStopped);
        }
        if self.hub_process.has_exited() {
            return Err(Stopped::HubGone);
        }
        Ok(())
    }

    /// Tells the hub, the first time, that this client waits for cycles now.
    fn mark_ready(&mut self) {
        if !self.ready {
            self.view
                .control()
                .client_flags
                .fetch_or(client_flags::READY, Ordering::Release);
            self.ready = true;
        }
    }

    /// The cycle the hub has woken this client for and it has not run yet.
    fn pending(&self) -> Option<u64> {
        let cycle = self.view.control().cycle.load(Ordering::Acquire);
        (cycle != 0 && cycle != self.last_cycle).then_some(cycle)
    }

    /// Runs `process` on the pending cycle, if there is one, without waiting.
    /// Returns whether it ran.
    ///
    /// The hub does not wake a client before its first call to this,
    /// [`Client::wait_cycle`] or [`Client::run`].
    pub fn try_cycle<F: FnOnce(&mut Cycle<'_>)>(&mut self, process: F) -> Result<bool, Stopped> {
        self.check_stop()?;
        self.mark_ready();
        match self.pending() {
            Some(cycle) => {
                self.run_one(cycle, process);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Waits up to `timeout` for the hub to wake this client, then runs
    /// `process` on the cycle. Returns whether it ran.
    pub fn wait_cycle<F: FnOnce(&mut Cycle<'_>)>(
        &mut self,
        timeout: Duration,
        process: F,
    ) -> Result<bool, Stopped> {
        self.mark_ready();
        let start = Instant::now();
        loop {
            let seen = self.go.seen();
            self.check_stop()?;
            if let Some(cycle) = self.pending() {
                self.run_one(cycle, process);
                return Ok(true);
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                self.check_hub()?;
                return Ok(false);
            }
            if !self.go.wait(&mut self.waiter, seen, timeout - elapsed) {
                // Timed out, or ended because the hub's process did.
                self.check_hub()?;
            }
        }
    }

    /// Runs `process` once a cycle until the hub stops or goes away.
    ///
    /// The calling thread is promoted to real-time priority while this runs
    /// (see `mare_sys::promote_current_thread`), and let go when it returns.
    /// A thread that calls [`Client::wait_cycle`] itself should do the same.
    pub fn run<F: FnMut(&mut Cycle<'_>)>(&mut self, mut process: F) -> Stopped {
        let _promoted = mare_sys::promote_current_thread().ok();
        loop {
            if let Err(stopped) = self.wait_cycle(Duration::from_millis(100), &mut process) {
                return stopped;
            }
        }
    }

    fn run_one<F: FnOnce(&mut Cycle<'_>)>(&mut self, number: u64, process: F) {
        let control = self.view.control();
        let max = self.view.layout().config().max_frames;
        let frames = control.frames.load(Ordering::Relaxed).min(max) as usize;
        let mut cycle = Cycle {
            number,
            frame_time: control.frame_time.load(Ordering::Relaxed),
            frames,
            view: &self.view,
            events_in: self.events_in.as_mut(),
            events_out: self.events_out.as_mut(),
        };
        for channel in 0..cycle.output_channels() {
            if let Some(out) = cycle.output(channel) {
                out.fill(0.0);
            }
        }
        process(&mut cycle);
        control.done_cycle.store(number, Ordering::Release);
        self.last_cycle = number;
        self.hub_bell.ring();
    }

    /// Leaves the hub. Dropping the client does the same.
    pub fn leave(self) {}
}

impl Drop for Client {
    fn drop(&mut self) {
        self.view
            .control()
            .client_flags
            .fetch_or(client_flags::LEAVING, Ordering::Release);
        give_up(self.slot_ref(), slot_state::ACCEPTED);
        self.keeper_bell.ring();
    }
}

/// One cycle, as a client sees it while it runs.
pub struct Cycle<'a> {
    number: u64,
    frame_time: u64,
    frames: usize,
    view: &'a SegmentView,
    events_in: Option<&'a mut RingConsumer>,
    events_out: Option<&'a mut RingProducer>,
}

impl Cycle<'_> {
    /// The cycle's number, counted from 1.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The master clock's frame at this cycle's first frame.
    pub fn frame_time(&self) -> u64 {
        self.frame_time
    }

    /// Frames in this cycle.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Audio channels into the client.
    pub fn input_channels(&self) -> u32 {
        self.view.layout().config().input_channels
    }

    /// Audio channels out of the client.
    pub fn output_channels(&self) -> u32 {
        self.view.layout().config().output_channels
    }

    /// This cycle's samples of input channel `channel`.
    pub fn input(&self, channel: u32) -> Option<&[f32]> {
        let start = self.view.input(channel)?;
        // SAFETY: the channel holds `max_frames >= frames` samples, and the
        // hub does not write it until this client marks the cycle done.
        Some(unsafe { std::slice::from_raw_parts(start.as_ptr(), self.frames) })
    }

    /// This cycle's samples of output channel `channel`, zero until written.
    pub fn output(&mut self, channel: u32) -> Option<&mut [f32]> {
        let start = self.view.output(channel)?;
        // SAFETY: as for `input`; the hub does not read it until this client
        // marks the cycle done, and `&mut self` makes the slice unique here.
        Some(unsafe { std::slice::from_raw_parts_mut(start.as_ptr(), self.frames) })
    }

    /// Input channel `input` and output channel `output` together, for a
    /// client that reads one while it writes the other.
    pub fn input_output(&mut self, input: u32, output: u32) -> Option<(&[f32], &mut [f32])> {
        let input = self.view.input(input)?;
        let output = self.view.output(output)?;
        // SAFETY: as for `input` and `output`; an input channel and an output
        // channel never overlap in the layout.
        Some(unsafe {
            (
                std::slice::from_raw_parts(input.as_ptr(), self.frames),
                std::slice::from_raw_parts_mut(output.as_ptr(), self.frames),
            )
        })
    }

    /// The next event the hub sent, oldest first. A ring the hub has
    /// corrupted is emptied and reads as empty.
    pub fn next_event(&mut self) -> Option<UmpEvent> {
        let ring = self.events_in.as_deref_mut()?;
        match ring.pop() {
            Ok(event) => event,
            Err(_) => {
                ring.discard_all();
                None
            }
        }
    }

    /// Sends `event` to the hub, or hands it back if the ring is full (or the
    /// client has no output ring).
    pub fn send_event(&mut self, event: UmpEvent) -> Result<(), UmpEvent> {
        match self.events_out.as_deref_mut() {
            Some(ring) => ring.push(event),
            None => Err(event),
        }
    }
}
