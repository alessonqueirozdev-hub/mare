//! The ratio controller against clocks that drift, over hours of simulated
//! time.
//!
//! A far device delivers packets on its own clock, which runs `drift` faster
//! than the master's; the hub consumes one period of output a cycle, which
//! takes `period × ratio` input frames from the queue. Only the queue's
//! level is simulated here (the audio itself is in `closed_loop.rs`), so
//! hours take seconds.
//!
//! The level the controller is given counts what the far device has recorded
//! but not yet delivered, as a hub estimates it from the time since the last
//! packet arrived (the queue alone moves a packet at a time, and the first
//! version of these tests showed the loop locking into a swing of hundreds
//! of ppm on that staircase). It is given exactly, and with an error of up to
//! ±30 frames, as a timestamp's jitter would make it.

use mare_drift::{ControllerSettings, RatioController};

const RATE: u32 = 48_000;
const PERIOD: usize = 128;
/// The queue is held here; the far device's packets are up to 480 frames.
const TARGET: f64 = 1_200.0;
/// What the resampler needs to make a period: its latency plus a period.
const FLOOR: f64 = 32.0 + 128.0;

struct Run {
    lowest: f64,
    highest: f64,
    settled_error: f64,
    settled_jitter: f64,
    final_drift: f64,
}

/// Simulates `seconds`, the far clock at `drift(t)` (a fraction; t in
/// seconds), delivering packets of `packet` frames.
fn simulate(seconds: f64, packet: usize, jitter: f64, drift: impl Fn(f64) -> f64) -> Run {
    let settings = ControllerSettings::new(TARGET, PERIOD, RATE);
    let mut controller = RatioController::new(settings);
    let cycles = (seconds * f64::from(RATE) / PERIOD as f64) as u64;
    let settle_after = (60.0 * f64::from(RATE) / PERIOD as f64) as u64;
    // The queue starts at the target, as after a device's start.
    let mut level = TARGET;
    let mut owed = 0.0;
    let mut run = Run {
        lowest: f64::MAX,
        highest: f64::MIN,
        settled_error: 0.0,
        settled_jitter: 0.0,
        final_drift: 0.0,
    };
    let (mut min_ratio, mut max_ratio) = (f64::MAX, f64::MIN);
    // A fixed pseudo-random sequence, so every run is the same.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    };
    for cycle in 0..cycles {
        let t = cycle as f64 * PERIOD as f64 / f64::from(RATE);
        // The far device: a period's worth of its own frames, in packets.
        owed += PERIOD as f64 * (1.0 + drift(t));
        while owed >= packet as f64 {
            level += packet as f64;
            owed -= packet as f64;
        }
        // The queue, plus what was recorded and not yet delivered.
        let ratio = controller.update(level + owed + jitter * noise());
        level -= PERIOD as f64 * ratio;
        run.lowest = run.lowest.min(level);
        run.highest = run.highest.max(level);
        if cycle >= settle_after {
            run.settled_error = run.settled_error.max((controller.drift() - drift(t)).abs());
            min_ratio = min_ratio.min(ratio);
            max_ratio = max_ratio.max(ratio);
        }
    }
    run.settled_jitter = max_ratio - min_ratio;
    run.final_drift = controller.drift();
    run
}

fn ppm(x: f64) -> f64 {
    x * 1e6
}

#[test]
fn two_hours_at_steady_drifts_never_starve_or_overflow() {
    // (timing jitter in frames, largest estimate error, largest ratio swing)
    let cases = [(0.0, 1.0, 1.0), (30.0, 5.0, 60.0)];
    for drift_ppm in [-300.0, -100.0, 0.0, 50.0, 500.0] {
        for packet in [128, 441, 480] {
            for (jitter, max_error, max_swing) in cases {
                let drift = drift_ppm * 1e-6;
                let run = simulate(2.0 * 3600.0, packet, jitter, |_| drift);
                eprintln!(
                    "{drift_ppm:+} ppm, packets of {packet}, jitter {jitter}: level {:.0}..{:.0}, estimate {:+.3} ppm, settled error {:.3} ppm, ratio swing {:.2} ppm",
                    run.lowest,
                    run.highest,
                    ppm(run.final_drift),
                    ppm(run.settled_error),
                    ppm(run.settled_jitter),
                );
                assert!(run.lowest > FLOOR, "starved: {:.0}", run.lowest);
                assert!(run.highest < TARGET + 1_000.0, "grew: {:.0}", run.highest);
                assert!(
                    ppm(run.settled_error) < max_error,
                    "estimate off by {:.3} ppm",
                    ppm(run.settled_error)
                );
                assert!(
                    ppm(run.settled_jitter) < max_swing,
                    "ratio swing {:.2} ppm",
                    ppm(run.settled_jitter)
                );
            }
        }
    }
}

#[test]
fn a_clock_that_wanders_with_temperature_is_followed() {
    // 100 ppm, wandering ±20 ppm over ten minutes, for two hours.
    let drift = |t: f64| 100e-6 + 20e-6 * (2.0 * std::f64::consts::PI * t / 600.0).sin();
    let run = simulate(2.0 * 3600.0, 480, 30.0, drift);
    eprintln!(
        "wandering: level {:.0}..{:.0}, tracking error {:.3} ppm",
        run.lowest,
        run.highest,
        ppm(run.settled_error)
    );
    assert!(run.lowest > FLOOR);
    assert!(run.highest < TARGET + 1_000.0);
    // Measured 5.2 ppm: about 3 from the timing jitter, the rest the lag of
    // a 20 s loop behind ±20 ppm over ten minutes.
    assert!(ppm(run.settled_error) < 8.0);
}

#[test]
fn a_sudden_change_of_drift_is_absorbed() {
    // The far device is swapped for one 400 ppm off at ten minutes.
    let drift = |t: f64| if t < 600.0 { 0.0 } else { 400e-6 };
    let run = simulate(1800.0, 480, 30.0, drift);
    eprintln!(
        "step: level {:.0}..{:.0}, final estimate {:+.3} ppm",
        run.lowest,
        run.highest,
        ppm(run.final_drift)
    );
    assert!(run.lowest > FLOOR);
    assert!(run.highest < TARGET + 1_000.0);
    assert!((ppm(run.final_drift) - 400.0).abs() < 1.0);
}

#[test]
fn drift_beyond_the_limit_is_held_at_the_limit() {
    let settings = ControllerSettings::new(TARGET, PERIOD, RATE);
    let mut controller = RatioController::new(settings);
    // A queue that only grows: the ratio rises to +1000 ppm and stops.
    for n in 0..1_000_000 {
        controller.update(TARGET + n as f64);
    }
    assert!((controller.ratio() - 1.001).abs() < 1e-12);
    assert!((controller.drift() - 1e-3).abs() < 1e-12);
}
