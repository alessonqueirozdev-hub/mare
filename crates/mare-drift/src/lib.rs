//! Drift correction for MARÉ.
//!
//! A device that is not the hub's master clock runs on its own crystal, a few
//! hundred parts per million away from the master's. Its audio crosses to the
//! master's clock through a queue and a [`Resampler`] whose ratio a
//! [`RatioController`] sets every cycle from the queue's level, so that the
//! queue neither empties nor grows without end (`docs/adr/0001`).
//!
//! A [`link`] puts the two together between two threads on two clocks: a
//! lock-free queue the producer pushes packets into, noting when each
//! arrived, and a consumer that resamples from it at its own pace.
//!
//! All of them allocate only when made; processing never allocates, locks
//! or makes a system call, so the hub's cycle thread can run them.

mod controller;
mod link;
mod resampler;

pub use controller::{ControllerSettings, RatioController};
pub use link::{LinkConsumer, LinkProducer, LinkSettings, LinkStats, link};
pub use resampler::Resampler;

/// The furthest the ratio is allowed from nominal: ±1000 ppm. A device
/// further off runs at another rate, which is configuration, not drift.
pub const MAX_DRIFT: f64 = 1e-3;
