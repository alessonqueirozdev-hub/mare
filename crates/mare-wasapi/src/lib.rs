//! A WASAPI device for MARÉ's hub.
//!
//! [`WasapiDevice`] opens a render endpoint for event-driven output, in one
//! of two modes:
//!
//! - [`Mode::Exclusive`]: the device's own format and its smallest period
//!   (or the one asked for), with no system mixer in between. Other programs
//!   cannot use the device while it is open.
//! - [`Mode::Shared`]: the system mixer's format, with the small periods
//!   `IAudioClient3` allows on Windows 10 and later. Other programs keep
//!   playing.
//!
//! The device implements `mare_hub::Device`: the hub's cycle thread waits on
//! the device's event, and hands it each period's mix, converted to the
//! device's format. Nothing on that path allocates or locks. The stream
//! starts on the first period, so on the cycle thread.
//!
//! A device may also have an input: a capture endpoint opened beside the
//! output ([`WasapiDevice::open_duplex`]), in the same mode and at the same
//! sample rate. The output paces the cycles; the input packets are queued
//! and handed to the hub a period at a time, with any padding or dropping
//! counted (see [`fifo`]).
//!
//! Everything but [`format`](mod@format) and [`fifo`] is Windows-only; elsewhere
//! only they are built.

// The crate's documentation links to its Windows-only items, which do not
// exist when it is built (and documented) elsewhere.
#![cfg_attr(not(windows), allow(rustdoc::broken_intra_doc_links))]

pub mod fifo;
pub mod format;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::WasapiDevice;

use std::fmt;

/// How a device is shared with other programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The system mixer's format, small periods, other programs keep playing.
    Shared,
    /// The device's own format and smallest period, for this program alone.
    Exclusive,
}

/// How to open a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenOptions {
    /// Shared or exclusive.
    pub mode: Mode,
    /// The period wanted, in frames; the nearest the device allows (rounding
    /// up) is used. `None` asks for the smallest.
    pub period_frames: Option<u32>,
    /// The sample rate wanted, in exclusive mode; `None` keeps the mixer's.
    /// Shared mode always uses the mixer's.
    pub sample_rate: Option<u32>,
}

impl OpenOptions {
    /// The smallest period, in `mode`, at the mixer's sample rate.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            period_frames: None,
            sample_rate: None,
        }
    }
}

/// An endpoint the system knows of, for output or for input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The endpoint id, which [`WasapiDevice::open`] and
    /// [`WasapiDevice::open_duplex`] take.
    pub id: String,
    /// Its name, for people to read.
    pub name: String,
    /// Whether it is the system default for its direction.
    pub is_default: bool,
}

/// What a device's latency is made of, in frames, as WASAPI reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Latency {
    /// Frames in each period the hub runs.
    pub period_frames: u32,
    /// Frames the device's buffer holds.
    pub buffer_frames: u32,
    /// The stream's own latency, as `IAudioClient::GetStreamLatency` gives
    /// it (the driver's figure; nothing here measures it).
    pub stream_latency_frames: u32,
}

/// Why a device could not be listed or opened.
#[derive(Debug)]
#[non_exhaustive]
pub enum WasapiError {
    /// No such device, or no default one.
    NoDevice,
    /// The device or the system lacks something MARÉ needs.
    Unsupported(&'static str),
    /// A call failed with this `HRESULT`.
    Os {
        /// What was being done.
        what: &'static str,
        /// The `HRESULT`.
        code: i32,
    },
}

impl fmt::Display for WasapiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no such audio device"),
            Self::Unsupported(what) => write!(f, "not supported: {what}"),
            Self::Os { what, code } => write!(f, "{what} failed ({:#010x})", *code as u32),
        }
    }
}

impl std::error::Error for WasapiError {}

/// The render endpoints that are active now.
#[cfg(windows)]
pub fn render_endpoints() -> Result<Vec<Endpoint>, WasapiError> {
    windows::render_endpoints()
}

/// The capture endpoints that are active now. Listing them opens none.
#[cfg(windows)]
pub fn capture_endpoints() -> Result<Vec<Endpoint>, WasapiError> {
    windows::capture_endpoints()
}

#[cfg(windows)]
impl WasapiDevice {
    /// Opens the render endpoint `id` (from [`render_endpoints`]).
    pub fn open(id: &str, options: &OpenOptions) -> Result<Self, WasapiError> {
        windows::open(Some(id), None, options)
    }

    /// Opens the render endpoint `output` with the capture endpoint `input`
    /// beside it (from [`capture_endpoints`]), in the same mode and at the
    /// same sample rate. The output paces the hub's cycles; each cycle takes
    /// one period of what the input recorded (see [`fifo`]).
    pub fn open_duplex(
        output: &str,
        input: &str,
        options: &OpenOptions,
    ) -> Result<Self, WasapiError> {
        windows::open(Some(output), Some(input), options)
    }

    /// Opens the system's default render endpoint.
    pub fn open_default(options: &OpenOptions) -> Result<Self, WasapiError> {
        windows::open(None, None, options)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::windows::shared_period;

    #[test]
    fn shared_periods_are_the_minimum_plus_whole_fundamentals() {
        // A typical engine: 48 kHz, minimum 144 frames (3 ms), fundamental
        // 48 (1 ms), maximum 480.
        assert_eq!(shared_period(None, 144, 480, 48), 144);
        assert_eq!(shared_period(Some(64), 144, 480, 48), 144);
        assert_eq!(shared_period(Some(150), 144, 480, 48), 192);
        assert_eq!(shared_period(Some(192), 144, 480, 48), 192);
        assert_eq!(shared_period(Some(4096), 144, 480, 48), 480);
    }
}
