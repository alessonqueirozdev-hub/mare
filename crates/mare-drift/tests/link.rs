//! A link between two clocks, simulated event by event and run on threads.
//!
//! In the simulation the producer delivers packets on its own clock, `drift`
//! fast, each arriving up to half a millisecond late (and stamped when it
//! arrives, as a thread woken by a device would stamp it); the consumer pulls
//! periods on the master's clock. A sine crosses, and every frame handed out
//! is checked against the sine at the position it came from.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use mare_drift::{LinkSettings, link};

const RATE: u32 = 48_000;

struct Outcome {
    rms_db: f64,
    worst_db: f64,
    starved: u64,
    overflowed: u64,
    drift: f64,
    lowest: f64,
    highest: f64,
}

fn simulate(drift: f64, packet: usize, period: usize, seconds: f64, freq: f64) -> Outcome {
    let omega = 2.0 * std::f64::consts::PI * freq / f64::from(RATE);
    let settings = LinkSettings::new(1, RATE, packet, period);
    let (mut producer, mut consumer) = link(settings);
    let producer_period = packet as f64 / (f64::from(RATE) * (1.0 + drift));
    let consumer_period = period as f64 / f64::from(RATE);
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut late = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 * 0.5e-3
    };
    let (mut next_packet, mut next_pull) = (late(), consumer_period);
    let mut recorded = 0usize;
    let mut packet_buf = vec![0.0f32; packet];
    let mut out = vec![0.0f32; period];
    let (mut sum, mut worst, mut counted) = (0.0f64, 0.0f64, 0usize);
    let (mut lowest, mut highest) = (f64::MAX, f64::MIN);
    let mut packets = 1u64;
    while next_pull < seconds {
        if next_packet <= next_pull {
            for (i, x) in packet_buf.iter_mut().enumerate() {
                *x = (omega * (recorded + i) as f64).sin() as f32;
            }
            producer.push(&packet_buf, (next_packet * 1e9) as u64);
            recorded += packet;
            next_packet = packets as f64 * producer_period + late();
            packets += 1;
        } else {
            let before = consumer.stats();
            let start = consumer.position();
            consumer.pull(&mut out, (next_pull * 1e9) as u64);
            let after = consumer.stats();
            let step = (consumer.position() - start) / period as f64;
            // Only frames that came from the queue are checked, after the
            // first few hundred, whose filter read the silence before the
            // first frame.
            if after.frames_out - before.frames_out == period as u64 && after.frames_out > 1_000 {
                for (i, &x) in out.iter().enumerate() {
                    let at = start + i as f64 * step;
                    let error = (f64::from(x) - (omega * at).sin()).abs();
                    sum += error * error;
                    worst = worst.max(error);
                    counted += 1;
                }
                lowest = lowest.min(after.level);
                highest = highest.max(after.level);
            }
            next_pull += consumer_period;
        }
    }
    let stats = consumer.stats();
    Outcome {
        rms_db: 20.0 * (sum / counted as f64).sqrt().log10(),
        worst_db: 20.0 * worst.log10(),
        starved: stats.starved,
        overflowed: stats.overflowed,
        drift: stats.drift,
        lowest,
        highest,
    }
}

#[test]
fn audio_crosses_clocks_through_a_link_without_a_seam() {
    // (drift, producer packet, consumer period): a capture device of 10 ms
    // packets into a hub of 128-frame cycles, and a hub of 144-frame cycles
    // into a device taking 480 at a time.
    for (drift, packet, period) in [(200e-6, 480, 128), (-400e-6, 144, 480), (0.0, 441, 64)] {
        let o = simulate(drift, packet, period, 30.0, 1_000.0);
        eprintln!(
            "{:+.0} ppm, packets {packet}, periods {period}: rms {:.1} dB, worst {:.1} dB, level {:.0}..{:.0}, starved {}, overflowed {}, estimate {:+.1} ppm",
            drift * 1e6,
            o.rms_db,
            o.worst_db,
            o.lowest,
            o.highest,
            o.starved,
            o.overflowed,
            o.drift * 1e6
        );
        assert_eq!(o.starved, 0);
        assert_eq!(o.overflowed, 0);
        assert!(o.rms_db < -90.0);
        assert!(o.worst_db < -80.0);
    }
}

#[test]
fn a_link_carries_audio_between_real_threads() {
    // A producer thread delivers 480-frame packets every 10 ms; a consumer
    // thread takes 128 frames every 2.67 ms; both stamp time from one clock.
    let omega = 2.0 * std::f64::consts::PI * 1_000.0 / f64::from(RATE);
    let (mut producer, mut consumer) = link(LinkSettings::new(1, RATE, 480, 128));
    let start = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(Barrier::new(2));
    let producing = {
        let (stop, go) = (Arc::clone(&stop), Arc::clone(&go));
        std::thread::spawn(move || {
            go.wait();
            let mut recorded = 0usize;
            let mut packet = vec![0.0f32; 480];
            let mut due = start;
            while !stop.load(Ordering::Acquire) {
                due += Duration::from_micros(10_000);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                for (i, x) in packet.iter_mut().enumerate() {
                    *x = (omega * (recorded + i) as f64).sin() as f32;
                }
                producer.push(&packet, start.elapsed().as_nanos() as u64);
                recorded += 480;
            }
        })
    };
    go.wait();
    let mut out = vec![0.0f32; 128];
    let (mut sum, mut counted, mut clean_pulls, mut pulls) = (0.0f64, 0usize, 0, 0);
    let mut due = start;
    while start.elapsed() < Duration::from_secs(3) {
        due += Duration::from_nanos(128 * 1_000_000_000 / u64::from(RATE));
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let before = consumer.stats();
        let at = consumer.position();
        consumer.pull(&mut out, start.elapsed().as_nanos() as u64);
        let after = consumer.stats();
        let step = (consumer.position() - at) / 128.0;
        pulls += 1;
        if after.frames_out - before.frames_out == 128 && after.frames_out > 1_000 {
            clean_pulls += 1;
            for (i, &x) in out.iter().enumerate() {
                let error = f64::from(x) - (omega * (at + i as f64 * step)).sin();
                sum += error * error;
                counted += 1;
            }
        }
    }
    stop.store(true, Ordering::Release);
    producing.join().unwrap();
    let rms_db = 20.0 * (sum / counted as f64).sqrt().log10();
    let stats = consumer.stats();
    eprintln!(
        "threads: {clean_pulls} of {pulls} pulls full, rms {rms_db:.1} dB, starved {} frames, overflowed {}",
        stats.starved, stats.overflowed
    );
    // Scheduling on a loaded machine may starve a pull now and then; what
    // the queue does hand out must be exact.
    assert!(clean_pulls > pulls / 2);
    assert!(rms_db < -90.0);
}
