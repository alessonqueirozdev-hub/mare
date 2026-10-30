//! The controller that sets a resampler's ratio from a queue's level.
//!
//! Once a cycle the hub tells the controller how many input frames wait in
//! the queue (the resampler's buffer included). The level is first smoothed
//! by a one-pole low-pass filter, which removes the jumps of whole packets
//! arriving at once; the difference between the smoothed level and the
//! target then drives a proportional-integral loop:
//!
//! ```text
//! error    = smoothed level − target          (frames)
//! integral = integral + ki · error
//! ratio    = nominal · (1 + kp · error + integral)
//! ```
//!
//! A queue that fills means the far device is faster, so the ratio rises and
//! the resampler consumes more. In the steady state the error is zero and the
//! integral is the clocks' relative drift. The gains follow from how fast the
//! loop should settle, for a critically damped loop: with *ω* the natural
//! frequency in radians a cycle and *P* the frames a cycle consumes,
//! `kp = 2ω / P` and `ki = ω² / P`, and *ω* = 4 / settling time in cycles.

use crate::MAX_DRIFT;

/// How a [`RatioController`] behaves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControllerSettings {
    /// The level the queue is held at, in input frames. It must exceed the
    /// largest packet the far device delivers plus the resampler's latency,
    /// with room for the loop's transient.
    pub target: f64,
    /// Output frames consumed per cycle.
    pub period: usize,
    /// Cycles a second.
    pub cycles_per_second: f64,
    /// About how long, in seconds, the loop takes to absorb a change of
    /// drift.
    pub settle_seconds: f64,
    /// The time constant, in seconds, of the low-pass filter on the level.
    pub smoothing_seconds: f64,
    /// The ratio when the clocks agree: input rate over output rate.
    pub nominal: f64,
}

impl ControllerSettings {
    /// Settings for a hub of `period` frames at `sample_rate`, holding the
    /// queue at `target` frames: 20 s to settle, a 1 s filter, nominal ratio
    /// 1 (both devices at the same nominal rate).
    pub fn new(target: f64, period: usize, sample_rate: u32) -> Self {
        Self {
            target,
            period,
            cycles_per_second: f64::from(sample_rate) / period as f64,
            settle_seconds: 20.0,
            smoothing_seconds: 1.0,
            nominal: 1.0,
        }
    }
}

/// Sets a resampler's ratio from the level of the queue before it.
#[derive(Clone, Debug)]
pub struct RatioController {
    settings: ControllerSettings,
    kp: f64,
    ki: f64,
    alpha: f64,
    smoothed: f64,
    integral: f64,
    ratio: f64,
    primed: bool,
}

impl RatioController {
    /// A controller with `settings`, its drift estimate at zero.
    pub fn new(settings: ControllerSettings) -> Self {
        let settle_cycles = settings.settle_seconds * settings.cycles_per_second;
        let omega = 4.0 / settle_cycles;
        let period = settings.period as f64;
        let smoothing_cycles = settings.smoothing_seconds * settings.cycles_per_second;
        Self {
            settings,
            kp: 2.0 * omega / period,
            ki: omega * omega / period,
            alpha: 1.0 - (-1.0 / smoothing_cycles.max(1.0)).exp(),
            smoothed: settings.target,
            integral: 0.0,
            ratio: settings.nominal,
            primed: false,
        }
    }

    /// Takes this cycle's queue level, in input frames, and returns the ratio
    /// to resample at.
    pub fn update(&mut self, level: f64) -> f64 {
        if !self.primed {
            // Start the filter at the first level seen, not at the target.
            self.smoothed = level;
            self.primed = true;
        }
        self.smoothed += self.alpha * (level - self.smoothed);
        let error = self.smoothed - self.settings.target;
        self.integral = (self.integral + self.ki * error).clamp(-MAX_DRIFT, MAX_DRIFT);
        let correction = (self.kp * error + self.integral).clamp(-MAX_DRIFT, MAX_DRIFT);
        self.ratio = self.settings.nominal * (1.0 + correction);
        self.ratio
    }

    /// The last ratio returned.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The estimated drift of the far clock against the near one: the
    /// integral term, as a fraction (1e-6 is one ppm).
    pub fn drift(&self) -> f64 {
        self.integral
    }

    /// The smoothed level.
    pub fn level(&self) -> f64 {
        self.smoothed
    }

    /// The settings.
    pub fn settings(&self) -> &ControllerSettings {
        &self.settings
    }
}
