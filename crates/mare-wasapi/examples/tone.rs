//! Plays a quiet sine wave through a real device, for trying MARÉ by hand.
//!
//! This is the one program in the repository that makes sound. It runs a hub
//! on the device you name and a client that plays a sine, then prints what
//! the device and the hub reported. Nothing automated runs it.
//!
//! ```text
//! cargo run -p mare-wasapi --example tone -- --device Realtek
//! cargo run -p mare-wasapi --example tone -- --device UMC --exclusive --level -30
//! ```
//!
//! Options:
//!
//! - `--device <part of its name>`: required; `--list` shows the names;
//! - `--exclusive`: exclusive mode (default shared);
//! - `--period <frames>`: the period wanted (default the smallest);
//! - `--level <dBFS>`: the sine's level, at most −6 (default −30, quiet);
//! - `--freq <Hz>`: the sine's frequency (default 440);
//! - `--seconds <s>`: how long to play (default 5).
//!
//! Turn the monitors or headphones down before the first run.

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use mare_client::{Client, ClientConfig};
    use mare_hub::{Clock, Device, HubConfig, SystemClock};
    use mare_sys::HubName;
    use mare_wasapi::{Mode, OpenOptions, WasapiDevice, render_endpoints};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let endpoints = render_endpoints().expect("listing devices");
    if args.iter().any(|a| a == "--list") {
        for e in &endpoints {
            println!("{}{}", e.name, if e.is_default { " (default)" } else { "" });
        }
        return;
    }
    let Some(part) = value("--device") else {
        eprintln!("name a device with --device (see --list)");
        std::process::exit(2);
    };
    let Some(endpoint) = endpoints
        .iter()
        .find(|e| e.name.to_lowercase().contains(&part.to_lowercase()))
    else {
        eprintln!("no device named like {part:?} (see --list)");
        std::process::exit(2);
    };
    let mode = if args.iter().any(|a| a == "--exclusive") {
        Mode::Exclusive
    } else {
        Mode::Shared
    };
    let level_db: f32 = value("--level").map_or(-30.0, |v| v.parse().expect("--level in dBFS"));
    let level_db = level_db.min(-6.0);
    let freq: f32 = value("--freq").map_or(440.0, |v| v.parse().expect("--freq in Hz"));
    let seconds: f64 = value("--seconds").map_or(5.0, |v| v.parse().expect("--seconds"));
    let options = OpenOptions {
        period_frames: value("--period").map(|v| v.parse().expect("--period in frames")),
        ..OpenOptions::new(mode)
    };

    let device = WasapiDevice::open(&endpoint.id, &options).expect("opening the device");
    let config = device.config();
    println!(
        "{}: {:?}, {:?}, {} Hz, {} channels",
        device.name(),
        device.mode(),
        device.sample_format(),
        config.sample_rate,
        config.output_channels
    );
    let latency = device.latency();
    let ms = |frames: u32| f64::from(frames) * 1000.0 / f64::from(config.sample_rate);
    println!(
        "period {} frames ({:.2} ms), buffer {} frames ({:.2} ms), stream latency {} frames ({:.2} ms, as the driver reports it)",
        latency.period_frames,
        ms(latency.period_frames),
        latency.buffer_frames,
        ms(latency.buffer_frames),
        latency.stream_latency_frames,
        ms(latency.stream_latency_frames),
    );
    println!("playing {freq} Hz at {level_db} dBFS for {seconds} s");

    let hub = HubName::new("tone").unwrap();
    let running = mare_hub::start(
        &HubConfig::new(hub.clone()),
        device,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
    )
    .expect("starting the hub");
    let rate = config.sample_rate as f32;
    let gain = 10f32.powf(level_db / 20.0);
    let player = std::thread::spawn(move || {
        let mut client =
            Client::connect(&hub, &ClientConfig::new("tone", 0, 2)).expect("connecting");
        client.run(|cycle| {
            let start = cycle.frame_time();
            let frames = cycle.frames();
            for channel in 0..cycle.output_channels() {
                let out = cycle.output(channel).unwrap();
                for (i, sample) in out.iter_mut().enumerate().take(frames) {
                    // The phase comes from the master clock's frame, so a
                    // missed cycle is a gap, never a shift in pitch.
                    let t = ((start + i as u64) % config.sample_rate as u64) as f32 / rate;
                    *sample = gain * (std::f32::consts::TAU * freq * t).sin();
                }
            }
        })
    });
    let until = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < until && !running.has_stopped() {
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("cycle thread promoted: {}", running.is_realtime());
    let (engine, error) = running.stop();
    let stats = engine.stats();
    drop(engine);
    let _ = player.join();
    if let Some(error) = error {
        println!("the device stopped: {error}");
    }
    println!(
        "{} cycles; the client finished {} in time, was late {} times and busy {}; longest wait {:.3} ms",
        stats.cycles,
        stats.client_cycles,
        stats.late,
        stats.busy,
        stats.longest_wait_ns as f64 / 1e6
    );
}

#[cfg(not(windows))]
fn main() {
    eprintln!("WASAPI is Windows-only");
}
