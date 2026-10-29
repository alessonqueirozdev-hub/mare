//! Shows an input's level as it reaches a client through the hub, for trying
//! duplex by hand.
//!
//! A hub runs on the output you name, with the input you name beside it, and
//! a client reads the input and plays silence. Four times a second the
//! loudest sample the client read is printed as a level in dBFS. Nothing is
//! recorded or kept. Nothing automated runs it.
//!
//! ```text
//! cargo run -p mare-wasapi --example meter -- --device Realtek --input Microfone
//! ```
//!
//! Options: `--device <part of an output's name>` and `--input <part of an
//! input's name>` (both required; the `list` example shows the names),
//! `--exclusive` (default shared), `--seconds <s>` (default 10).

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use mare_client::{Client, ClientConfig};
    use mare_hub::{Clock, Device, HubConfig, SystemClock};
    use mare_sys::HubName;
    use mare_wasapi::{
        Endpoint, Mode, OpenOptions, WasapiDevice, capture_endpoints, render_endpoints,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let find = |list: Vec<Endpoint>, option: &str| {
        let Some(part) = value(option) else {
            eprintln!("name one with {option} (see the list example)");
            std::process::exit(2);
        };
        list.into_iter()
            .find(|e| e.name.to_lowercase().contains(&part.to_lowercase()))
            .unwrap_or_else(|| {
                eprintln!("nothing named like {part:?} for {option}");
                std::process::exit(2);
            })
    };
    let output = find(render_endpoints().expect("listing outputs"), "--device");
    let input = find(capture_endpoints().expect("listing inputs"), "--input");
    let mode = if args.iter().any(|a| a == "--exclusive") {
        Mode::Exclusive
    } else {
        Mode::Shared
    };
    let seconds: f64 = value("--seconds").map_or(10.0, |v| v.parse().expect("--seconds"));

    let device = WasapiDevice::open_duplex(&output.id, &input.id, &OpenOptions::new(mode))
        .expect("opening the devices");
    let config = device.config();
    println!(
        "{} + {}: {:?}, {} Hz, {} in, {} out, period {} frames ({:.2} ms)",
        device.name(),
        device.input_name().unwrap_or_default(),
        mode,
        config.sample_rate,
        config.input_channels,
        config.output_channels,
        config.period_frames,
        f64::from(config.period_frames) * 1000.0 / f64::from(config.sample_rate)
    );

    let hub = HubName::new("meter").unwrap();
    let running = mare_hub::start(
        &HubConfig::new(hub.clone()),
        device,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
    )
    .expect("starting the hub");
    // The loudest magnitude since the last print, as f32 bits: for numbers
    // that are not negative, their bits order as they do.
    let peak = Arc::new(AtomicU32::new(0));
    let client_peak = Arc::clone(&peak);
    let inputs = config.input_channels;
    let listener = std::thread::spawn(move || {
        let mut client =
            Client::connect(&hub, &ClientConfig::new("meter", inputs, 2)).expect("connecting");
        client.run(|cycle| {
            let mut loudest = 0.0f32;
            for channel in 0..cycle.input_channels() {
                for x in cycle.input(channel).unwrap() {
                    loudest = loudest.max(x.abs());
                }
            }
            client_peak.fetch_max(loudest.to_bits(), Ordering::Relaxed);
            for channel in 0..cycle.output_channels() {
                cycle.output(channel).unwrap().fill(0.0);
            }
        })
    });

    println!("speak, tap or clap near the input:");
    let until = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < until && !running.has_stopped() {
        std::thread::sleep(Duration::from_millis(250));
        let level = f32::from_bits(peak.swap(0, Ordering::Relaxed));
        let db = 20.0 * level.max(1e-6).log10();
        let bar = ((db + 60.0).max(0.0) / 1.5) as usize;
        println!("{db:7.1} dBFS |{}", "#".repeat(bar.min(40)));
    }
    let (engine, error) = running.stop();
    let stats = engine.stats();
    let input_stats = engine.device().input_stats();
    drop(engine);
    let _ = listener.join();
    if let Some(error) = error {
        println!("the device stopped: {error}");
    }
    println!(
        "{} cycles; the client finished {} in time, late {} busy {}",
        stats.cycles, stats.client_cycles, stats.late, stats.busy
    );
    if let Some(input) = input_stats {
        println!(
            "input: {} frames delivered, {} handed on, {} padded, {} dropped, {} discontinuities",
            input.frames_in, input.frames_out, input.padded, input.dropped, input.discontinuities
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("WASAPI is Windows-only");
}
