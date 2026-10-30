//! Audio across two clocks, through the resampler with the loop closed.
//!
//! A far device records a sine on its own clock, `drift` faster than the
//! master's, and delivers it in packets. Each cycle the hub feeds the
//! controller the queue's level (with what was recorded but not yet
//! delivered), resamples one period at the ratio it returns, and checks every
//! output frame against the sine at that frame's position in the far
//! device's recording: an error near the resampler's own floor means no
//! frame was dropped, repeated or put out of place, however the ratio moved.

use mare_drift::{ControllerSettings, RatioController, Resampler};

const RATE: u32 = 48_000;
const PERIOD: usize = 128;
const PACKET: usize = 480;
const TARGET: f64 = 1_200.0;

fn run(drift: f64, seconds: f64, freq: f64) {
    let omega = 2.0 * std::f64::consts::PI * freq / f64::from(RATE);
    let mut resampler = Resampler::with_defaults(1, 4 * PACKET);
    let mut controller = RatioController::new(ControllerSettings::new(TARGET, PERIOD, RATE));
    let mut recorded = 0usize;
    let mut owed = 0.0;
    let mut packet = vec![0.0f32; PACKET];
    let mut out = vec![0.0f32; PERIOD];
    // Fill to the target first, as a device's start would.
    while resampler.buffered() < TARGET {
        for (i, x) in packet.iter_mut().enumerate() {
            *x = (omega * (recorded + i) as f64).sin() as f32;
        }
        resampler.push(&packet);
        recorded += PACKET;
    }
    let cycles = (seconds * f64::from(RATE) / PERIOD as f64) as usize;
    let (mut sum, mut worst, mut counted) = (0.0f64, 0.0f64, 0usize);
    let (mut lowest, mut highest) = (f64::MAX, f64::MIN);
    for cycle in 0..cycles {
        owed += PERIOD as f64 * (1.0 + drift);
        while owed >= PACKET as f64 {
            for (i, x) in packet.iter_mut().enumerate() {
                *x = (omega * (recorded + i) as f64).sin() as f32;
            }
            assert_eq!(
                resampler.push(&packet),
                PACKET,
                "queue full at cycle {cycle}"
            );
            recorded += PACKET;
            owed -= PACKET as f64;
        }
        let level = resampler.buffered() + owed;
        lowest = lowest.min(resampler.buffered());
        highest = highest.max(resampler.buffered());
        resampler.set_ratio(controller.update(level));
        for x in out.iter_mut() {
            let at = resampler.position();
            assert_eq!(
                resampler.pull(std::slice::from_mut(x)),
                1,
                "starved at cycle {cycle}"
            );
            // The first frames' filter reads the silence before the first
            // recorded frame, which the exact sine does not have.
            if cycle * PERIOD >= 200 {
                let error = (f64::from(*x) - (omega * at).sin()).abs();
                sum += error * error;
                worst = worst.max(error);
                counted += 1;
            }
        }
    }
    let rms = (sum / counted as f64).sqrt();
    eprintln!(
        "{:+} ppm, {freq} Hz, {seconds} s: rms {:.1} dB, worst {:.1} dB, queue {lowest:.0}..{highest:.0}, estimate {:+.2} ppm",
        drift * 1e6,
        20.0 * rms.log10(),
        20.0 * worst.log10(),
        controller.drift() * 1e6,
    );
    assert!(20.0 * rms.log10() < -90.0);
    assert!(20.0 * worst.log10() < -80.0);
}

#[test]
fn a_sine_crosses_from_a_fast_clock_without_a_seam() {
    run(300e-6, 40.0, 1_000.0);
}

#[test]
fn a_sine_crosses_from_a_slow_clock_without_a_seam() {
    run(-500e-6, 40.0, 5_000.0);
}
