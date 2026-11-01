//! MARÉ's client library for C and C++.
//!
//! This crate builds `mare.dll` (with its import library `mare.dll.lib`) on
//! Windows, `libmare.so` on Linux and `libmare.dylib` on macOS, plus static
//! libraries, exporting the functions `include/mare_client.h` declares:
//! connect to a hub, run its cycles from a callback, read and write audio,
//! and send and receive events. `include/mare_client.hpp` wraps them for
//! C++.
//!
//! Every function here is a thin layer over [`mare_client`]; the cycle path
//! (`mare_wait_cycle`, `mare_try_cycle`, `mare_run` and the `mare_cycle_*`
//! accessors) allocates nothing and locks nothing. A panic inside the
//! library aborts the process rather than unwind into C; a callback must not
//! unwind into the library either (a C++ callback catches its exceptions).

use std::ffi::{CStr, c_char, c_void};
use std::time::Duration;

use mare_client::{Client, ClientConfig, ConnectError, Cycle, Stopped};
use mare_protocol::ring::UmpEvent;
use mare_sys::{HubName, RealtimeThread};

/// The version of the C interface: the major version in the high 16 bits,
/// the minor in the low. A program built against a header of another major
/// version must not use the library.
pub const MARE_ABI_VERSION: u32 = 0x0000_0001;

/// The call succeeded (and, for a cycle call, the cycle ran).
pub const MARE_OK: i32 = 0;
/// No hub of that name is running.
pub const MARE_NO_HUB: i32 = 1;
/// The hub is starting or stopping.
pub const MARE_HUB_NOT_RUNNING: i32 = 2;
/// The hub's registry has another layout (another version of MARÉ).
pub const MARE_BAD_REGISTRY: i32 = 3;
/// The configuration asks for something the protocol cannot hold.
pub const MARE_BAD_CONFIG: i32 = 4;
/// The hub has no free slot.
pub const MARE_FULL: i32 = 5;
/// The hub refused the client.
pub const MARE_REJECTED: i32 = 6;
/// The hub did not answer in time.
pub const MARE_TIMEOUT: i32 = 7;
/// The operating system refused something.
pub const MARE_IO: i32 = 8;
/// The hub is shutting down, or has dropped this client.
pub const MARE_HUB_STOPPED: i32 = 9;
/// The hub's process has ended.
pub const MARE_HUB_GONE: i32 = 10;
/// No cycle was pending (or none came before the timeout).
pub const MARE_NO_CYCLE: i32 = 11;
/// An argument was null, not UTF-8, or out of range.
pub const MARE_BAD_ARGUMENT: i32 = 12;

/// What a client asks of the hub, as C sees it (`mare_client_config`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MareClientConfig {
    /// `sizeof(mare_client_config)` as the caller compiled it, so that later
    /// versions can add fields. Set by `mare_client_config_init`.
    pub size: u32,
    /// Audio channels into the client.
    pub input_channels: u32,
    /// Audio channels out of the client.
    pub output_channels: u32,
    /// Capacity of the ring of events into the client: zero or a power of
    /// two.
    pub events_in_capacity: u32,
    /// Capacity of the ring of events out of the client: zero or a power of
    /// two.
    pub events_out_capacity: u32,
    /// The latency the client adds, in frames.
    pub latency_frames: u32,
    /// How long to wait for the hub to answer, in milliseconds.
    pub connect_timeout_ms: u32,
    /// Zero.
    pub reserved: u32,
    /// A name for people to read: UTF-8, at most 64 bytes, nul-terminated.
    pub name: *const c_char,
}

/// How the hub runs (`mare_hub_info`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MareHubInfo {
    /// Frames per second.
    pub sample_rate: u32,
    /// Frames in each cycle.
    pub period_frames: u32,
    /// The longest cycle, in frames.
    pub max_frames: u32,
    /// The hub's process id.
    pub hub_pid: u32,
}

/// An event (`mare_event`): a Universal MIDI Packet of up to four words and
/// the frame of the cycle it belongs to.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MareEvent {
    /// The frame, on the hub's clock.
    pub time: u64,
    /// The packet's words; those past its size are ignored.
    pub words: [u32; 4],
}

/// A connected client (`mare_client`), opaque to C.
pub struct MareClient(Client);

/// A cycle in progress (`mare_cycle`), opaque to C, valid only inside the
/// callback it is handed to.
pub struct MareCycle {
    _private: [u8; 0],
}

/// A thread raised to real-time priority (`mare_realtime`), opaque to C.
pub struct MareRealtime(RealtimeThread);

/// The callback that runs a cycle (`mare_process_fn`).
pub type MareProcess = Option<unsafe extern "C" fn(cycle: *mut MareCycle, user: *mut c_void)>;

/// The interface's version, `MARE_ABI_VERSION` of the library.
#[unsafe(no_mangle)]
pub extern "C" fn mare_abi_version() -> u32 {
    MARE_ABI_VERSION
}

/// A short English description of `status`, a static nul-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn mare_status_text(status: i32) -> *const c_char {
    let text: &CStr = match status {
        MARE_OK => c"ok",
        MARE_NO_HUB => c"no hub of that name is running",
        MARE_HUB_NOT_RUNNING => c"the hub is not running",
        MARE_BAD_REGISTRY => c"the hub's registry is not understood",
        MARE_BAD_CONFIG => c"invalid client configuration",
        MARE_FULL => c"the hub has no free slot",
        MARE_REJECTED => c"the hub refused the client",
        MARE_TIMEOUT => c"the hub did not answer in time",
        MARE_IO => c"the operating system refused something",
        MARE_HUB_STOPPED => c"the hub stopped",
        MARE_HUB_GONE => c"the hub's process ended",
        MARE_NO_CYCLE => c"no cycle was pending",
        MARE_BAD_ARGUMENT => c"invalid argument",
        _ => c"unknown status",
    };
    text.as_ptr()
}

/// Fills `config` with the defaults: no channels, rings of 256 events each
/// way, no latency, two seconds to connect, no name.
///
/// # Safety
///
/// `config` is null or points to a writable `mare_client_config`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_client_config_init(config: *mut MareClientConfig) {
    // SAFETY: the caller's guarantee.
    let Some(config) = (unsafe { config.as_mut() }) else {
        return;
    };
    let defaults = ClientConfig::new("", 0, 0);
    *config = MareClientConfig {
        size: size_of::<MareClientConfig>() as u32,
        input_channels: 0,
        output_channels: 0,
        events_in_capacity: defaults.events_in_capacity,
        events_out_capacity: defaults.events_out_capacity,
        latency_frames: defaults.latency_frames,
        connect_timeout_ms: defaults.connect_timeout.as_millis() as u32,
        reserved: 0,
        name: std::ptr::null(),
    };
}

fn connect_status(error: &ConnectError) -> i32 {
    match error {
        ConnectError::NoHub(_) => MARE_NO_HUB,
        ConnectError::HubNotRunning => MARE_HUB_NOT_RUNNING,
        ConnectError::BadRegistry(_) => MARE_BAD_REGISTRY,
        ConnectError::Config(_) => MARE_BAD_CONFIG,
        ConnectError::Full => MARE_FULL,
        ConnectError::Rejected(_) => MARE_REJECTED,
        ConnectError::Timeout => MARE_TIMEOUT,
        _ => MARE_IO,
    }
}

fn stopped_status(stopped: Stopped) -> i32 {
    match stopped {
        Stopped::HubGone => MARE_HUB_GONE,
        _ => MARE_HUB_STOPPED,
    }
}

/// Reads a nul-terminated UTF-8 string, or `None` if it is not one.
///
/// # Safety
///
/// `text` points to a nul-terminated string.
unsafe fn utf8<'a>(text: *const c_char) -> Option<&'a str> {
    // SAFETY: the caller's guarantee.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// Connects to the hub called `hub` (or the default hub, if `hub` is null)
/// and stores the client in `*client`. On failure `*client` is null.
///
/// # Safety
///
/// `hub` is null or a nul-terminated string; `config` points to a
/// `mare_client_config` filled by `mare_client_config_init` and then
/// changed, whose `name` is null or a nul-terminated string; `client`
/// points to a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_connect(
    hub: *const c_char,
    config: *const MareClientConfig,
    client: *mut *mut MareClient,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(out) = (unsafe { client.as_mut() }) else {
        return MARE_BAD_ARGUMENT;
    };
    *out = std::ptr::null_mut();
    // SAFETY: the caller's guarantee.
    let Some(config) = (unsafe { config.as_ref() }) else {
        return MARE_BAD_ARGUMENT;
    };
    if (config.size as usize) < size_of::<MareClientConfig>() {
        return MARE_BAD_ARGUMENT;
    }
    let hub = if hub.is_null() {
        HubName::default_hub()
    } else {
        // SAFETY: the caller's guarantee.
        match unsafe { utf8(hub) }.map(HubName::new) {
            Some(Ok(name)) => name,
            _ => return MARE_BAD_ARGUMENT,
        }
    };
    let name = if config.name.is_null() {
        ""
    } else {
        // SAFETY: the caller's guarantee.
        match unsafe { utf8(config.name) } {
            Some(name) => name,
            None => return MARE_BAD_ARGUMENT,
        }
    };
    let rust = ClientConfig {
        events_in_capacity: config.events_in_capacity,
        events_out_capacity: config.events_out_capacity,
        latency_frames: config.latency_frames,
        connect_timeout: Duration::from_millis(config.connect_timeout_ms.into()),
        ..ClientConfig::new(name, config.input_channels, config.output_channels)
    };
    match Client::connect(&hub, &rust) {
        Ok(connected) => {
            *out = Box::into_raw(Box::new(MareClient(connected)));
            MARE_OK
        }
        Err(error) => connect_status(&error),
    }
}

/// Leaves the hub and frees the client. A null `client` is ignored.
///
/// # Safety
///
/// `client` is null or was returned by `mare_connect` and not left yet; no
/// other thread is using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_leave(client: *mut MareClient) {
    if !client.is_null() {
        // SAFETY: the caller's guarantee: it came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(client) });
    }
}

/// Stores how the hub runs in `*info`.
///
/// # Safety
///
/// `client` is a connected client and `info` points to a writable
/// `mare_hub_info`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_client_info(
    client: *const MareClient,
    info: *mut MareHubInfo,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let (Some(client), Some(info)) = (unsafe { client.as_ref() }, unsafe { info.as_mut() }) else {
        return MARE_BAD_ARGUMENT;
    };
    let hub = client.0.info();
    *info = MareHubInfo {
        sample_rate: hub.sample_rate,
        period_frames: hub.period_frames,
        max_frames: hub.max_frames,
        hub_pid: hub.hub_pid,
    };
    MARE_OK
}

/// The last cycle this client finished, or zero.
///
/// # Safety
///
/// `client` is a connected client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_client_last_cycle(client: *const MareClient) -> u64 {
    // SAFETY: the caller's guarantee.
    unsafe { client.as_ref() }.map_or(0, |c| c.0.last_cycle())
}

/// Cycles the hub says this client missed.
///
/// # Safety
///
/// `client` is a connected client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_client_late_cycles(client: *const MareClient) -> u64 {
    // SAFETY: the caller's guarantee.
    unsafe { client.as_ref() }.map_or(0, |c| c.0.late_cycles())
}

/// Runs `process` on `cycle`.
///
/// # Safety
///
/// `process` is a valid callback for `user`.
unsafe fn call(process: MareProcess, cycle: &mut Cycle<'_>, user: *mut c_void) {
    if let Some(process) = process {
        let cycle = std::ptr::from_mut(cycle).cast::<MareCycle>();
        // SAFETY: the caller's guarantee; the pointer is valid for the call.
        unsafe { process(cycle, user) };
    }
}

/// Waits up to `timeout_ms` for the hub to wake this client and runs
/// `process(cycle, user)` on the cycle: `MARE_OK` if it ran, `MARE_NO_CYCLE`
/// if none came in time, `MARE_HUB_STOPPED` or `MARE_HUB_GONE` if the hub
/// has stopped or gone. A null `process` runs the cycle with silent outputs.
///
/// # Safety
///
/// `client` is a connected client used by no other thread; `process` is
/// null or a function that may be called with `user`, and that neither
/// unwinds nor keeps the cycle pointer after it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_wait_cycle(
    client: *mut MareClient,
    timeout_ms: u32,
    process: MareProcess,
    user: *mut c_void,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(client) = (unsafe { client.as_mut() }) else {
        return MARE_BAD_ARGUMENT;
    };
    let timeout = Duration::from_millis(timeout_ms.into());
    // SAFETY: the caller's guarantee about `process` and `user`.
    let process = |cycle: &mut Cycle<'_>| unsafe { call(process, cycle, user) };
    match client.0.wait_cycle(timeout, process) {
        Ok(true) => MARE_OK,
        Ok(false) => MARE_NO_CYCLE,
        Err(stopped) => stopped_status(stopped),
    }
}

/// Runs `process(cycle, user)` on the pending cycle, if there is one,
/// without waiting: `MARE_OK` if it ran, `MARE_NO_CYCLE` if none was
/// pending, `MARE_HUB_STOPPED` if the hub has stopped.
///
/// # Safety
///
/// As for `mare_wait_cycle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_try_cycle(
    client: *mut MareClient,
    process: MareProcess,
    user: *mut c_void,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(client) = (unsafe { client.as_mut() }) else {
        return MARE_BAD_ARGUMENT;
    };
    // SAFETY: the caller's guarantee about `process` and `user`.
    let process = |cycle: &mut Cycle<'_>| unsafe { call(process, cycle, user) };
    match client.0.try_cycle(process) {
        Ok(true) => MARE_OK,
        Ok(false) => MARE_NO_CYCLE,
        Err(stopped) => stopped_status(stopped),
    }
}

/// Runs `process(cycle, user)` once a cycle, on the calling thread raised
/// to real-time priority, until the hub stops or goes away; returns
/// `MARE_HUB_STOPPED` or `MARE_HUB_GONE`. A program that must stop the
/// client itself loops on `mare_wait_cycle` instead.
///
/// # Safety
///
/// As for `mare_wait_cycle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_run(
    client: *mut MareClient,
    process: MareProcess,
    user: *mut c_void,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(client) = (unsafe { client.as_mut() }) else {
        return MARE_BAD_ARGUMENT;
    };
    // SAFETY: the caller's guarantee about `process` and `user`.
    stopped_status(client.0.run(|cycle| unsafe { call(process, cycle, user) }))
}

/// Raises the calling thread to real-time priority (MMCSS "Pro Audio" on
/// Windows, `SCHED_FIFO` on Linux within the user's limit, a time-constraint
/// policy on macOS). Returns a handle to give back with
/// `mare_realtime_release` on the same thread, or null if the system could
/// not be asked.
#[unsafe(no_mangle)]
pub extern "C" fn mare_promote_current_thread() -> *mut MareRealtime {
    match mare_sys::promote_current_thread() {
        Ok(promoted) => Box::into_raw(Box::new(MareRealtime(promoted))),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Whether the system raised the thread's priority: 1 or 0.
///
/// # Safety
///
/// `realtime` is null or was returned by `mare_promote_current_thread`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_realtime_is_promoted(realtime: *const MareRealtime) -> i32 {
    // SAFETY: the caller's guarantee.
    unsafe { realtime.as_ref() }.map_or(0, |r| i32::from(r.0.is_promoted()))
}

/// Lowers the thread to its earlier priority and frees the handle. A null
/// `realtime` is ignored.
///
/// # Safety
///
/// `realtime` is null or was returned by `mare_promote_current_thread` on
/// this same thread and not released yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_realtime_release(realtime: *mut MareRealtime) {
    if !realtime.is_null() {
        // SAFETY: the caller's guarantee: it came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(realtime) });
    }
}

/// The cycle behind a `mare_cycle` pointer.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
unsafe fn cycle_mut<'a>(cycle: *mut MareCycle) -> Option<&'a mut Cycle<'a>> {
    // SAFETY: the caller's guarantee: it points to the live `Cycle`.
    unsafe { cycle.cast::<Cycle<'a>>().as_mut() }
}

/// The cycle's number, counted from 1.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_number(cycle: *const MareCycle) -> u64 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle.cast_mut()) }.map_or(0, |c| c.number())
}

/// The hub's frame at the cycle's first frame.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_frame_time(cycle: *const MareCycle) -> u64 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle.cast_mut()) }.map_or(0, |c| c.frame_time())
}

/// Frames in the cycle.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_frames(cycle: *const MareCycle) -> u32 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle.cast_mut()) }.map_or(0, |c| c.frames() as u32)
}

/// Audio channels into the client.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_input_channels(cycle: *const MareCycle) -> u32 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle.cast_mut()) }.map_or(0, |c| c.input_channels())
}

/// Audio channels out of the client.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_output_channels(cycle: *const MareCycle) -> u32 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle.cast_mut()) }.map_or(0, |c| c.output_channels())
}

/// The cycle's `mare_cycle_frames` samples of input channel `channel`, or
/// null if there is no such channel. Valid until the callback returns.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_input(cycle: *const MareCycle, channel: u32) -> *const f32 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle.cast_mut()) }
        .and_then(|c| c.input(channel))
        .map_or(std::ptr::null(), <[f32]>::as_ptr)
}

/// The cycle's `mare_cycle_frames` samples of output channel `channel`,
/// zero until written, or null if there is no such channel. Valid until the
/// callback returns.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_output(cycle: *mut MareCycle, channel: u32) -> *mut f32 {
    // SAFETY: the caller's guarantee.
    unsafe { cycle_mut(cycle) }
        .and_then(|c| c.output(channel))
        .map_or(std::ptr::null_mut(), <[f32]>::as_mut_ptr)
}

/// Takes the next event the hub sent into `*event`: 1 if there was one,
/// 0 if not.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running; `event` points to a
/// writable `mare_event`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_next_event(
    cycle: *mut MareCycle,
    event: *mut MareEvent,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let (Some(cycle), Some(event)) = (unsafe { cycle_mut(cycle) }, unsafe { event.as_mut() })
    else {
        return 0;
    };
    match cycle.next_event() {
        Some(next) => {
            *event = MareEvent {
                time: next.time,
                words: next.words,
            };
            1
        }
        None => 0,
    }
}

/// Sends `*event` to the hub: 1 if it was queued, 0 if the ring is full or
/// the client has none.
///
/// # Safety
///
/// `cycle` was handed to the callback that is running; `event` points to a
/// `mare_event`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mare_cycle_send_event(
    cycle: *mut MareCycle,
    event: *const MareEvent,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let (Some(cycle), Some(event)) = (unsafe { cycle_mut(cycle) }, unsafe { event.as_ref() })
    else {
        return 0;
    };
    let event = UmpEvent {
        time: event.time,
        words: event.words,
    };
    i32::from(cycle.send_event(event).is_ok())
}
