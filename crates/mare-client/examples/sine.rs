//! Plays a quiet sine wave through a running hub, for trying MARÉ by hand.
//!
//! Start a hub first (`mare run`), then:
//!
//! ```text
//! cargo run -p mare-client --example sine
//! cargo run -p mare-client --example sine -- --hub organ --level -24 --seconds 10
//! ```
//!
//! Options:
//!
//! - `--hub <name>`: the hub to play through (default `main`);
//! - `--level <dBFS>`: the sine's level, at most −6 (default −30, quiet);
//! - `--freq <Hz>`: its frequency (default 440);
//! - `--channels <n>`: how many output channels play it (default 2);
//! - `--seconds <s>`: how long to play (default 5).
//!
//! Turn the monitors or headphones down before the first run.

use std::time::{Duration, Instant};

use mare_client::{Client, ClientConfig};
use mare_sys::HubName;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let hub = value("--hub").map_or_else(HubName::default_hub, |name| {
        HubName::new(&name).expect("a hub name: 1 to 10 of a-z, 0-9 and '-'")
    });
    let level_db: f32 = value("--level").map_or(-30.0, |v| v.parse().expect("--level in dBFS"));
    let level_db = level_db.min(-6.0);
    let freq: f32 = value("--freq").map_or(440.0, |v| v.parse().expect("--freq in Hz"));
    let channels: u32 = value("--channels").map_or(2, |v| v.parse().expect("--channels"));
    let seconds: f64 = value("--seconds").map_or(5.0, |v| v.parse().expect("--seconds"));

    let mut client = match Client::connect(&hub, &ClientConfig::new("sine", 0, channels)) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("connecting to hub {}: {error}", hub.as_str());
            std::process::exit(1);
        }
    };
    let info = client.info();
    println!(
        "hub {}: {} Hz, period {} frames; playing {freq} Hz at {level_db} dBFS on {channels} \
         channels for {seconds} s",
        hub.as_str(),
        info.sample_rate,
        info.period_frames
    );
    let rate = info.sample_rate;
    let gain = 10f32.powf(level_db / 20.0);
    let promoted = mare_sys::promote_current_thread().ok();
    let until = Instant::now() + Duration::from_secs_f64(seconds);
    let mut cycles = 0u64;
    while Instant::now() < until {
        let ran = client.wait_cycle(Duration::from_millis(100), |cycle| {
            let start = cycle.frame_time();
            let frames = cycle.frames();
            for channel in 0..cycle.output_channels() {
                let out = cycle.output(channel).unwrap();
                for (i, sample) in out.iter_mut().enumerate().take(frames) {
                    // The phase comes from the hub's frame, so a missed
                    // cycle is a gap, never a shift in pitch.
                    let t = ((start + i as u64) % u64::from(rate)) as f32 / rate as f32;
                    *sample = gain * (std::f32::consts::TAU * freq * t).sin();
                }
            }
        });
        match ran {
            Ok(true) => cycles += 1,
            Ok(false) => {}
            Err(stopped) => {
                println!("the hub stopped: {stopped:?}");
                break;
            }
        }
    }
    println!(
        "{cycles} cycles played, {} late; real-time priority: {}",
        client.late_cycles(),
        promoted.as_ref().is_some_and(|p| p.is_promoted())
    );
    client.leave();
}
