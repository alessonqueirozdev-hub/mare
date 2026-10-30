//! MARÉ's hub.
//!
//! The hub publishes a registry under its name, adopts the clients that
//! connect through it, and runs one cycle per period of its device. A cycle
//! wakes every client with a deadline and mixes the outputs of those that
//! finish in time. A client that misses the deadline loses its output for
//! that cycle, which is counted, and the others carry on; a client still
//! working on an earlier cycle is not woken again until it finishes; a client
//! whose process ends is noticed and its resources freed.
//!
//! The hub is two halves that never wait for each other:
//!
//! - the [`Engine`], which runs the cycles on a real-time thread and neither
//!   allocates, frees nor locks;
//! - the [`Housekeeper`], which adopts and frees clients on an ordinary
//!   thread.
//!
//! [`create`] makes both, for a caller (a test) that drives them by hand.
//! [`start`] makes both and runs each on its own thread.
//!
//! The graph is the first version's: every client reads the device's input
//! channels (channel *n* of the client from channel *n* of the device, and
//! silence past the device's last), and its outputs are summed into the
//! device's output channels the same way. Events a client sends are read and
//! counted, not yet routed anywhere.

mod clock;
mod device;
mod engine;
pub mod fake;
mod keeper;
mod shared;

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use mare_protocol::LayoutError;
use mare_protocol::registry::{RegistryHeader, RegistryLayout, RegistryView, hub_state};
use mare_sys::{Doorbell, HubName, ProcessWatch, SharedMemory, current_pid};

pub use clock::{Clock, FakeClock, SystemClock};
pub use device::{AudioBlock, Device, DeviceConfig, DeviceError};
pub use engine::{ClientId, CycleReport, Engine};
pub use keeper::{CLAIM_GRACE, Housekeeper};
pub use mare_protocol::segment::MAX_FRAMES;
pub use shared::Stats;

use shared::{Counters, HubSlot, Shared};

/// How a hub runs.
#[derive(Clone, Debug, PartialEq)]
pub struct HubConfig {
    /// The hub's name, which clients connect to.
    pub name: HubName,
    /// The most clients it runs at once (1 to 1024).
    pub max_clients: u32,
    /// The part of a period, from 0 to 1, that clients have to finish a
    /// cycle; the rest is the hub's, to mix and hand the result to the
    /// device.
    pub client_budget: f64,
    /// How long the housekeeping thread of [`start`] sleeps when no client
    /// rings it. Clients ring it when they connect or leave; the interval
    /// bounds how late a client whose process ended is noticed.
    pub housekeeping_interval: Duration,
}

impl HubConfig {
    /// A hub called `name` for up to 64 clients, which have 70 % of each
    /// period.
    pub fn new(name: HubName) -> Self {
        Self {
            name,
            max_clients: 64,
            client_budget: 0.7,
            housekeeping_interval: Duration::from_millis(20),
        }
    }
}

/// Why a hub could not start.
#[derive(Debug)]
#[non_exhaustive]
pub enum HubError {
    /// A hub of the same name is running.
    Exists,
    /// The configuration or the device's is out of range.
    Config(&'static str),
    /// The registry has no layout for this configuration.
    Layout(LayoutError),
    /// The operating system refused something.
    Io(io::Error),
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exists => write!(f, "a hub of the same name is running"),
            Self::Config(what) => write!(f, "invalid configuration: {what}"),
            Self::Layout(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HubError {}

impl From<io::Error> for HubError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Creates the registry under `name`, or reports that a running hub has it.
///
/// Where names outlive their users (not Windows), a registry left by a hub
/// that crashed is removed first.
fn create_registry(name: &HubName, size: usize) -> Result<SharedMemory, HubError> {
    match SharedMemory::create(&name.registry(), size) {
        Ok(memory) => return Ok(memory),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let stale = match SharedMemory::open(&name.registry()) {
        Ok(memory) => {
            // SAFETY: `memory` maps `len` bytes.
            match unsafe { RegistryHeader::read_from(memory.base(), memory.len() as u64) } {
                Ok(header) => ProcessWatch::open(header.hub_pid).map_or(true, |w| w.has_exited()),
                Err(_) => true,
            }
        }
        Err(_) => true,
    };
    if !stale {
        return Err(HubError::Exists);
    }
    SharedMemory::unlink(&name.registry())?;
    SharedMemory::create(&name.registry(), size).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            HubError::Exists
        } else {
            e.into()
        }
    })
}

/// Creates a hub on `device`, timed by `clock`, and returns its two halves
/// for the caller to drive.
pub fn create<D: Device>(
    config: &HubConfig,
    device: D,
    clock: Arc<dyn Clock>,
) -> Result<(Engine<D>, Housekeeper), HubError> {
    let device_config = device.config();
    if device_config.sample_rate == 0 {
        return Err(HubError::Config("sample rate is zero"));
    }
    if device_config.period_frames == 0
        || device_config.period_frames > device_config.max_frames
        || device_config.max_frames > MAX_FRAMES
    {
        return Err(HubError::Config("period out of range"));
    }
    if !(config.client_budget > 0.0 && config.client_budget < 1.0) {
        return Err(HubError::Config("client budget must be between 0 and 1"));
    }
    let layout = RegistryLayout::new(config.max_clients).map_err(HubError::Layout)?;
    let registry = create_registry(&config.name, layout.size() as usize)?;
    // SAFETY: the mapping is at least `layout.size()` bytes and is kept in
    // `Shared` with the view.
    let view = unsafe { RegistryView::from_raw(registry.base(), registry.len() as u64, layout) }
        .map_err(HubError::Layout)?;
    let header = layout.header(
        device_config.sample_rate,
        device_config.period_frames,
        device_config.max_frames,
        current_pid(),
    );
    // SAFETY: nobody trusts the header before RUNNING, stored below.
    unsafe { view.write_header(&header) };
    // SAFETY: the bell is in the registry, kept with the doorbell.
    let hub_bell = unsafe {
        Doorbell::create(
            std::ptr::NonNull::from(&view.control().hub_bell),
            &config.name.hub_wake(),
        )
    }?;
    // SAFETY: as above.
    let keeper_bell = unsafe {
        Doorbell::create(
            std::ptr::NonNull::from(&view.control().keeper_bell),
            &config.name.keeper_wake(),
        )
    }?;
    let shared = Arc::new(Shared {
        slots: (0..config.max_clients).map(|_| HubSlot::new()).collect(),
        hub_bell,
        keeper_bell,
        view,
        header,
        name: config.name.clone(),
        counters: Counters::default(),
        _registry: registry,
    });
    let period = Duration::from_nanos(
        (u64::from(device_config.period_frames) * 1_000_000_000)
            / u64::from(device_config.sample_rate),
    );
    let budget = period.mul_f64(config.client_budget);
    let engine = Engine::new(Arc::clone(&shared), device, clock, budget)?;
    let keeper = Housekeeper::new(Arc::clone(&shared));
    view.control()
        .hub_state
        .store(hub_state::RUNNING, Ordering::Release);
    Ok((engine, keeper))
}

/// A hub running on its own threads.
pub struct RunningHub<D: Device> {
    stop: Arc<AtomicBool>,
    engine: Option<JoinHandle<(Engine<D>, Option<DeviceError>)>>,
    keeper: Option<JoinHandle<Housekeeper>>,
    shared: Arc<Shared>,
    realtime: Arc<AtomicBool>,
}

impl<D: Device> fmt::Debug for RunningHub<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunningHub")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Creates a hub and runs its cycle thread and its housekeeping thread.
pub fn start<D: Device + 'static>(
    config: &HubConfig,
    device: D,
    clock: Arc<dyn Clock>,
) -> Result<RunningHub<D>, HubError> {
    let (mut engine, mut keeper) = create(config, device, clock)?;
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::clone(engine.shared());
    let engine_stop = Arc::clone(&stop);
    let realtime = Arc::new(AtomicBool::new(false));
    let engine_realtime = Arc::clone(&realtime);
    let engine = std::thread::Builder::new()
        .name("mare-cycle".into())
        .spawn(move || {
            // Kept for the thread's life. Failing to raise the priority is
            // not fatal; `RunningHub::is_realtime` reports it.
            let promoted = mare_sys::promote_current_thread().ok();
            let raised = promoted.as_ref().is_some_and(|p| p.is_promoted());
            engine_realtime.store(raised, Ordering::Release);
            while !engine_stop.load(Ordering::Acquire) {
                if let Err(error) = engine.run_cycle() {
                    return (engine, Some(error));
                }
            }
            (engine, None)
        })?;
    let keeper_stop = Arc::clone(&stop);
    let interval = config.housekeeping_interval;
    let keeper = std::thread::Builder::new()
        .name("mare-housekeeping".into())
        .spawn(move || {
            while !keeper_stop.load(Ordering::Acquire) {
                keeper.poll();
                keeper.wait(interval);
            }
            keeper
        })?;
    Ok(RunningHub {
        stop,
        engine: Some(engine),
        keeper: Some(keeper),
        shared,
        realtime,
    })
}

impl<D: Device> RunningHub<D> {
    /// The hub's counters.
    pub fn stats(&self) -> Stats {
        self.shared.counters.snapshot()
    }

    /// Whether the cycle thread runs at a raised, real-time priority (see
    /// `mare_sys::promote_current_thread`). False until the thread has
    /// started, and always false where MARÉ cannot raise it yet.
    pub fn is_realtime(&self) -> bool {
        self.realtime.load(Ordering::Acquire)
    }

    /// Whether the cycle thread has stopped by itself (its device failed).
    pub fn has_stopped(&self) -> bool {
        self.engine.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops both threads and returns the engine, with its device, and the
    /// device's error if that is what stopped it. Clients are told the hub
    /// has stopped.
    pub fn stop(mut self) -> (Engine<D>, Option<DeviceError>) {
        self.stop.store(true, Ordering::Release);
        self.shared.keeper_bell.ring();
        let (engine, error) = self
            .engine
            .take()
            .expect("stopped once")
            .join()
            .expect("the cycle thread does not panic");
        engine.shut_down();
        if let Some(keeper) = self.keeper.take() {
            let _ = keeper.join();
        }
        (engine, error)
    }
}

impl<D: Device> Drop for RunningHub<D> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.shared.keeper_bell.ring();
        if let Some(engine) = self.engine.take() {
            let _ = engine.join();
        }
        if let Some(keeper) = self.keeper.take() {
            let _ = keeper.join();
        }
    }
}
