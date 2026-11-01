//! Several fake devices as one, on clocks that disagree.
//!
//! On a fake clock the test runs every device's periods in the order their
//! own clocks put them, with no threads, and checks that a sine recorded by a
//! fast or slow follower reaches the hub exactly where the follower recorded
//! it, and that a sine the hub plays reaches the follower the same way. On
//! real threads it runs a hub on an aggregate and checks that the follower
//! kept up.

mod common;

use std::sync::Arc;
use std::time::Duration;

use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{
    AudioBlock, Clock, Device, DeviceConfig, FakeClock, HubConfig, RateMismatch, SystemClock,
    aggregate,
};

const RATE: u32 = 48_000;
const FREQ: f64 = 1_000.0;

fn omega() -> f64 {
    2.0 * std::f64::consts::PI * FREQ / f64::from(RATE)
}

fn tone(_channel: u32, frame: u64) -> f32 {
    (omega() * frame as f64).sin() as f32
}

fn level(channel: u32, _frame: u64) -> f32 {
    0.1 * (channel + 1) as f32
}

fn mono(period: u32) -> DeviceConfig {
    DeviceConfig {
        sample_rate: RATE,
        period_frames: period,
        max_frames: period,
        input_channels: 1,
        output_channels: 1,
    }
}

/// The error of a pull against the sine, summed.
#[derive(Default)]
struct Error {
    sum: f64,
    worst: f64,
    counted: usize,
}

impl Error {
    fn add(&mut self, got: &[f32], at: f64, step: f64) {
        for (i, &x) in got.iter().enumerate() {
            let e = (f64::from(x) - (omega() * (at + i as f64 * step)).sin()).abs();
            self.sum += e * e;
            self.worst = self.worst.max(e);
            self.counted += 1;
        }
    }

    fn rms_db(&self) -> f64 {
        20.0 * (self.sum / self.counted as f64).sqrt().log10()
    }

    fn worst_db(&self) -> f64 {
        20.0 * self.worst.log10()
    }
}

fn run(skew: f64, master_period: u32, follower_period: u32, seconds: f64) {
    let clock = Arc::new(FakeClock::new());
    let master = FakeDevice::new(mono(master_period), Pacing::Fake(Arc::clone(&clock)));
    let follower = FakeDevice::new(mono(follower_period), Pacing::Fake(Arc::clone(&clock)))
        .with_input(tone)
        .with_skew(skew);
    let (mut hub_side, followers) =
        aggregate(master, vec![follower], Arc::clone(&clock) as Arc<dyn Clock>).unwrap();
    let mut follower = followers.into_iter().next().unwrap();
    let config = hub_side.config();
    assert_eq!((config.input_channels, config.output_channels), (2, 2));

    let stride = master_period as usize;
    let mut inputs = vec![0.0f32; 2 * stride];
    let mut outputs = vec![0.0f32; 2 * stride];
    let (mut capture, mut playback) = (Error::default(), Error::default());
    let (mut master_periods, mut follower_periods) = (0u64, 0u64);
    let follower_rate = f64::from(RATE) * (1.0 + skew);
    loop {
        let master_at = (master_periods * u64::from(master_period)) as f64 / f64::from(RATE);
        let follower_at = (follower_periods * u64::from(follower_period)) as f64 / follower_rate;
        if master_at.min(follower_at) >= seconds {
            break;
        }
        if master_at <= follower_at {
            let (before, at) = (
                hub_side.capture_stats(0).unwrap(),
                hub_side.capture_position(0).unwrap(),
            );
            let frames = hub_side.wait_period().unwrap();
            hub_side
                .read(&mut AudioBlock::new(&mut inputs, 2, stride, frames))
                .unwrap();
            let after = hub_side.capture_stats(0).unwrap();
            let step = (hub_side.capture_position(0).unwrap() - at) / frames as f64;
            // Checked once the follower's audio flows, past the frames whose
            // filter read the silence before its first.
            if after.frames_out - before.frames_out == frames as u64 && after.frames_out > 1_000 {
                capture.add(&inputs[stride..stride + frames], at, step);
            }
            let first = master_periods * u64::from(master_period);
            for i in 0..frames {
                outputs[i] = 0.0;
                outputs[stride + i] = tone(1, first + i as u64);
            }
            hub_side
                .write(&AudioBlock::new(&mut outputs, 2, stride, frames))
                .unwrap();
            master_periods += 1;
        } else {
            let (before, at) = (
                follower.playback_stats().unwrap(),
                follower.playback_position().unwrap(),
            );
            follower.run_period().unwrap();
            let after = follower.playback_stats().unwrap();
            let frames = follower_period as usize;
            let step = (follower.playback_position().unwrap() - at) / frames as f64;
            if after.frames_out - before.frames_out == frames as u64 && after.frames_out > 1_000 {
                playback.add(follower.device().last_output(0), at, step);
            }
            follower_periods += 1;
        }
    }
    let (c, p) = (
        hub_side.capture_stats(0).unwrap(),
        follower.playback_stats().unwrap(),
    );
    eprintln!(
        "{:+.0} ppm, master {master_period}, follower {follower_period}: capture {:.1} dB rms, {:.1} dB worst, starved {}, estimate {:+.1} ppm, latency {:.0}; playback {:.1} dB rms, {:.1} dB worst, starved {}, estimate {:+.1} ppm, latency {:.0}",
        skew * 1e6,
        capture.rms_db(),
        capture.worst_db(),
        c.starved,
        c.drift * 1e6,
        hub_side.capture_latency(0).unwrap(),
        playback.rms_db(),
        playback.worst_db(),
        p.starved,
        p.drift * 1e6,
        follower.playback_latency().unwrap(),
    );
    for stats in [c, p] {
        assert_eq!(stats.starved, 0);
        assert_eq!(stats.overflowed, 0);
    }
    for error in [&capture, &playback] {
        assert!(error.rms_db() < -90.0);
        assert!(error.worst_db() < -80.0);
    }
    // What the follower records runs `skew` fast against the hub; what the
    // hub plays, `skew` slow against the follower.
    assert!((c.drift - skew).abs() < 50e-6);
    assert!((p.drift + skew / (1.0 + skew)).abs() < 50e-6);
}

#[test]
fn a_fast_follower_is_heard_and_fed_without_a_seam() {
    run(300e-6, 128, 480, 30.0);
}

#[test]
fn a_slow_follower_with_short_periods_is_heard_and_fed_without_a_seam() {
    run(-500e-6, 480, 64, 30.0);
}

#[test]
fn channels_are_the_masters_then_each_followers_in_order() {
    let clock = Arc::new(FakeClock::new());
    let master = FakeDevice::new(
        DeviceConfig {
            input_channels: 2,
            output_channels: 2,
            ..mono(128)
        },
        Pacing::Fake(Arc::clone(&clock)),
    )
    .with_input(level);
    let records = FakeDevice::new(
        DeviceConfig {
            output_channels: 0,
            ..mono(256)
        },
        Pacing::Fake(Arc::clone(&clock)),
    )
    .with_input(level);
    let plays = FakeDevice::new(
        DeviceConfig {
            input_channels: 0,
            output_channels: 3,
            ..mono(256)
        },
        Pacing::Fake(Arc::clone(&clock)),
    );
    let (mut hub_side, mut followers) = aggregate(
        master,
        vec![records, plays],
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .unwrap();
    let config = hub_side.config();
    assert_eq!((config.input_channels, config.output_channels), (3, 5));
    assert_eq!(hub_side.followers(), 2);
    assert!(hub_side.capture_stats(0).is_some() && hub_side.capture_stats(1).is_none());
    assert!(followers[0].playback_stats().is_none() && followers[1].playback_stats().is_some());

    let mut inputs = vec![0.0f32; 3 * 128];
    let mut outputs: Vec<f32> = (0..5 * 128).map(|i| (i / 128) as f32).collect();
    // Two master periods to each follower period, all on time, for a second.
    for period in 0..375 {
        let frames = hub_side.wait_period().unwrap();
        hub_side
            .read(&mut AudioBlock::new(&mut inputs, 3, 128, frames))
            .unwrap();
        hub_side
            .write(&AudioBlock::new(&mut outputs, 5, 128, frames))
            .unwrap();
        assert!(inputs[..128].iter().all(|&x| x == 0.1));
        assert!(inputs[128..256].iter().all(|&x| x == 0.2));
        assert!(hub_side.master().last_output(0).iter().all(|&x| x == 0.0));
        assert!(hub_side.master().last_output(1).iter().all(|&x| x == 1.0));
        if period % 2 == 0 {
            for follower in &mut followers {
                follower.run_period().unwrap();
            }
        }
    }
    // The recording follower's one channel came third, once it flowed; the
    // playing follower got the hub's channels 2 to 4.
    assert!(hub_side.capture_stats(0).unwrap().frames_out > 0);
    assert!(inputs[256..].iter().all(|&x| (x - 0.1).abs() < 1e-4));
    for (channel, expected) in [(0, 2.0), (1, 3.0), (2, 4.0)] {
        let played = followers[1].device().last_output(channel);
        assert!(played.iter().all(|&x| (x - expected).abs() < 1e-3));
    }
}

#[test]
fn a_follower_at_another_rate_is_refused() {
    let clock = Arc::new(FakeClock::new());
    let master = FakeDevice::new(mono(128), Pacing::Fake(Arc::clone(&clock)));
    let other = FakeDevice::new(
        DeviceConfig {
            sample_rate: 44_100,
            ..mono(128)
        },
        Pacing::Fake(Arc::clone(&clock)),
    );
    let refused = aggregate(master, vec![other], clock as Arc<dyn Clock>).err();
    assert_eq!(
        refused,
        Some(RateMismatch {
            index: 0,
            rate: 44_100,
            master: RATE
        })
    );
}

#[test]
fn a_hub_runs_on_an_aggregate_with_its_follower_on_a_thread() {
    // A 64-frame master and a follower 500 ppm slow with 10 ms periods, in
    // real time, for two seconds.
    let master = FakeDevice::new(
        DeviceConfig {
            max_frames: 128,
            ..mono(64)
        },
        Pacing::RealTime,
    );
    let follower = FakeDevice::new(mono(480), Pacing::RealTime)
        .with_input(tone)
        .with_skew(-500e-6);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let (hub_side, followers) = aggregate(master, vec![follower], clock.clone()).unwrap();
    let thread = followers.into_iter().next().unwrap().spawn().unwrap();
    let name = common::unique_hub();
    let hub = mare_hub::start(&HubConfig::new(name), hub_side, clock).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let (engine, error) = hub.stop();
    assert!(error.is_none());
    let (follower, error) = thread.stop();
    assert!(error.is_none());
    let (c, p) = (
        engine.device().capture_stats(0).unwrap(),
        follower.playback_stats().unwrap(),
    );
    eprintln!(
        "threads: master {} periods, follower {} periods; capture out {} priming {} starved {}; playback out {} priming {} starved {}",
        engine.device().master().periods(),
        follower.device().periods(),
        c.frames_out,
        c.priming,
        c.starved,
        p.frames_out,
        p.priming,
        p.starved
    );
    // Both directions flowed for most of the two seconds; a loaded machine
    // may starve a period now and then.
    assert!(c.frames_out > 48_000 && p.frames_out > 48_000);
    assert_eq!(c.overflowed + p.overflowed, 0);
}
