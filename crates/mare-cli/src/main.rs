//! The `mare` program: lists audio devices, runs a hub on one, and shows
//! running hubs. `mare help` says how.

use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mare_cli::args::{NULL_PERIOD, NULL_RATE, USAGE};
use mare_cli::{AnyDevice, Command, DeviceChoice, Report, RunOptions, Session, show};
use mare_client::ConnectError;
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, DeviceLatency, HubConfig, LatencySource};
use mare_sys::HubName;

fn main() -> ExitCode {
    match mare_cli::parse(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("mare {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Devices) => devices(),
        Ok(Command::Probe(hub)) => probe(&hub),
        Ok(Command::Run(options)) => run(&options),
        Err(error) => {
            eprintln!("mare: {error}\n`mare help` shows the commands and their options");
            ExitCode::from(2)
        }
    }
}

fn fail(message: impl Display) -> ExitCode {
    eprintln!("mare: {message}");
    ExitCode::FAILURE
}

#[cfg(windows)]
fn devices() -> ExitCode {
    for (title, list) in [
        ("Outputs", mare_wasapi::render_endpoints()),
        ("Inputs", mare_wasapi::capture_endpoints()),
    ] {
        match list {
            Ok(endpoints) => {
                println!("{title}:");
                for endpoint in &endpoints {
                    println!("{}", show::endpoint(endpoint));
                }
            }
            Err(error) => return fail(format!("listing the devices: {error}")),
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(windows))]
fn devices() -> ExitCode {
    println!("{NO_DEVICES}");
    ExitCode::SUCCESS
}

#[cfg(not(windows))]
const NO_DEVICES: &str = "MARÉ drives no audio devices on this system yet (only WASAPI's, on \
     Windows); `mare run --null` runs a hub on no device.";

fn probe(hub: &HubName) -> ExitCode {
    match mare_client::probe(hub) {
        Ok(info) => {
            println!("{}", show::hub_info(hub, &info));
            ExitCode::SUCCESS
        }
        Err(ConnectError::NoHub(_)) => fail(format!("no hub called {} is running", hub.as_str())),
        Err(error) => fail(format!("hub {}: {error}", hub.as_str())),
    }
}

/// The width the status line is padded to, so that a shorter one hides a
/// longer one.
const STATUS_WIDTH: usize = 100;

fn run(options: &RunOptions) -> ExitCode {
    let interrupted = match mare_sys::interrupt_flag() {
        Ok(flag) => flag,
        Err(error) => return fail(format!("catching Ctrl+C: {error}")),
    };
    let (master, followers) = match open_devices(options) {
        Ok(devices) => devices,
        Err(error) => return fail(error),
    };
    let device = master.config();
    let mut config = HubConfig::new(options.hub.clone());
    config.max_clients = options.max_clients;
    let mut session = match Session::start(&config, master, followers) {
        Ok(session) => session,
        Err(error) => return fail(format!("starting hub {}: {error}", options.hub.as_str())),
    };
    // The hub's channels, the followers' included, as programs see them.
    let (inputs, outputs) = mare_client::probe(&options.hub)
        .map_or((device.input_channels, device.output_channels), |info| {
            (info.device_inputs, info.device_outputs)
        });
    println!(
        "hub {} running: {} Hz, period {} frames ({:.2} ms), {} inputs, {} outputs{}",
        options.hub.as_str(),
        device.sample_rate,
        device.period_frames,
        show::ms(f64::from(device.period_frames), device.sample_rate),
        inputs,
        outputs,
        match session.followers() {
            0 => String::new(),
            n => format!(" (the first device's, then {n} followers')"),
        }
    );
    if let Some((input, output)) = options.measured_latency {
        if input.is_none() && inputs > 0 {
            session.stop();
            return fail(
                "the hub has inputs: give --input-latency with --output-latency, \
                 both from the measurement",
            );
        }
        session.set_latency(
            DeviceLatency {
                input_frames: input.unwrap_or(0),
                output_frames: output,
            },
            LatencySource::Measured,
        );
    }
    if let Ok(info) = mare_client::probe(&options.hub) {
        println!("{}", show::latency(&info.latency, device.sample_rate));
    }
    println!("programs connect to it by its name; Ctrl+C stops it");

    let started = Instant::now();
    let deadline = options
        .seconds
        .map(|s| started + Duration::from_secs_f64(s));
    let live = std::io::stdout().is_terminal();
    let mut stdout = std::io::stdout();
    let mut last = session.stats();
    let mut next_status = started;
    let mut status_shown = false;
    let mut priority_checked = false;
    while !interrupted.load(Ordering::Acquire)
        && !session.has_stopped()
        && deadline.is_none_or(|d| Instant::now() < d)
    {
        std::thread::sleep(Duration::from_millis(50));
        let now = session.stats();
        let mut lines = show::client_changes(&last, &now);
        last = now;
        for index in session.newly_stopped_followers() {
            lines.push(format!(
                "follower {} stopped; the hub carries on without it",
                index + 1
            ));
        }
        if !priority_checked && started.elapsed() > Duration::from_millis(500) {
            priority_checked = true;
            if !session.is_realtime() {
                lines.push(
                    "warning: the cycle thread runs at ordinary priority, so it may miss \
                     periods under load"
                        .to_owned(),
                );
            }
            let followers = session.followers();
            if session.realtime_followers() < followers {
                lines.push(format!(
                    "warning: {} of {followers} followers run at ordinary priority",
                    followers - session.realtime_followers()
                ));
            }
        }
        if !lines.is_empty() {
            if status_shown {
                print!("\r{:STATUS_WIDTH$}\r", "");
                status_shown = false;
            }
            for line in lines {
                println!("{line}");
            }
        }
        if live && Instant::now() >= next_status {
            print!(
                "\r{:STATUS_WIDTH$}",
                show::status_line(&now, started.elapsed())
            );
            let _ = stdout.flush();
            status_shown = true;
            next_status += Duration::from_secs(1);
        }
    }
    if status_shown {
        println!();
    }
    let why = if session.has_stopped() {
        "the device stopped"
    } else if interrupted.load(Ordering::Acquire) {
        "stopping (Ctrl+C)"
    } else {
        "stopping (time is up)"
    };
    println!("{why}");
    let running = started.elapsed();
    let report = session.stop();
    summarize(&report, running, device.sample_rate)
}

fn summarize(report: &Report, running: Duration, rate: u32) -> ExitCode {
    let stats = &report.stats;
    println!("{}", show::status_line(stats, running));
    println!(
        "clients: {} joined, {} left, {} ended without leaving, {} refused; \
         {} cycles finished in time",
        stats.accepted, stats.left, stats.died, stats.rejected, stats.client_cycles
    );
    for (index, follower) in report.followers.iter().enumerate() {
        let number = index + 1;
        if let Some(capture) = &follower.capture {
            println!(
                "follower {number} inputs: {}",
                show::link(capture, follower.capture_latency, rate)
            );
        }
        if let Some(playback) = &follower.playback {
            println!(
                "follower {number} outputs: {}",
                show::link(playback, follower.playback_latency, rate)
            );
        }
        if let Some(error) = &follower.error {
            println!("follower {number} stopped: {error}");
        }
    }
    match &report.error {
        Some(error) => fail(format!("the device stopped: {error}")),
        None => ExitCode::SUCCESS,
    }
}

/// Opens the device the hub runs on and its followers, and says what they
/// are.
fn open_devices(options: &RunOptions) -> Result<(AnyDevice, Vec<AnyDevice>), String> {
    match &options.device {
        DeviceChoice::Null { inputs, outputs } => {
            let period = options.period.unwrap_or(NULL_PERIOD);
            let config = DeviceConfig {
                sample_rate: options.rate.unwrap_or(NULL_RATE),
                period_frames: period,
                max_frames: period,
                input_channels: *inputs,
                output_channels: *outputs,
            };
            println!(
                "null device: silence in, nothing out, timed by the system clock, \
                 {} Hz, period {} frames ({:.2} ms)",
                config.sample_rate,
                period,
                show::ms(f64::from(period), config.sample_rate)
            );
            let device: AnyDevice = Box::new(FakeDevice::new(config, Pacing::RealTime));
            Ok((device, Vec::new()))
        }
        DeviceChoice::Default => open_wasapi(options, None),
        DeviceChoice::Named(name) => open_wasapi(options, Some(name)),
    }
}

#[cfg(windows)]
fn open_wasapi(
    options: &RunOptions,
    wanted: Option<&str>,
) -> Result<(AnyDevice, Vec<AnyDevice>), String> {
    use mare_hub::Device;
    use mare_wasapi::{Mode, OpenOptions, WasapiDevice, capture_endpoints, render_endpoints};

    let mode = if options.exclusive {
        Mode::Exclusive
    } else {
        Mode::Shared
    };
    let open = OpenOptions {
        period_frames: options.period,
        sample_rate: options.rate,
        ..OpenOptions::new(mode)
    };
    let outputs = render_endpoints().map_err(|e| format!("listing the devices: {e}"))?;
    let output = match wanted {
        Some(name) => show::find_endpoint(&outputs, name)?,
        None => outputs
            .iter()
            .find(|e| e.is_default)
            .ok_or("the system has no default output")?,
    };
    let master = match &options.input {
        None => WasapiDevice::open(&output.id, &open),
        Some(name) => {
            let inputs = capture_endpoints().map_err(|e| format!("listing the devices: {e}"))?;
            let input = show::find_endpoint(&inputs, name)?;
            WasapiDevice::open_duplex(&output.id, &input.id, &open)
        }
    }
    .map_err(|e| format!("opening {}: {e}", output.name))?;
    describe(&master);
    // Followers run at the master's rate: in exclusive mode they are asked
    // for it, in shared mode their mixers must already be at it.
    let rate = master.config().sample_rate;
    let follower_options = OpenOptions {
        sample_rate: options.exclusive.then_some(rate),
        ..open
    };
    let mut followers: Vec<AnyDevice> = Vec::new();
    for name in &options.followers {
        let endpoint = show::find_endpoint(&outputs, name)?;
        if endpoint.id == output.id {
            return Err(format!(
                "{} is the hub's own device; a follower is another one",
                endpoint.name
            ));
        }
        let device = WasapiDevice::open(&endpoint.id, &follower_options)
            .map_err(|e| format!("opening follower {}: {e}", endpoint.name))?;
        print!("follower {}: ", followers.len() + 1);
        describe(&device);
        followers.push(Box::new(device));
    }
    Ok((Box::new(master), followers))
}

#[cfg(windows)]
fn describe(device: &mare_wasapi::WasapiDevice) {
    use mare_hub::Device;

    let config = device.config();
    let latency = device.latency_parts();
    let ms = |frames: u32| show::ms(f64::from(frames), config.sample_rate);
    println!(
        "{}: {:?}, {:?}, {} Hz, {} outputs; period {} frames ({:.2} ms), buffer {} frames \
         ({:.2} ms), stream latency {} frames ({:.2} ms, as the driver reports it)",
        device.name(),
        device.mode(),
        device.sample_format(),
        config.sample_rate,
        config.output_channels,
        latency.period_frames,
        ms(latency.period_frames),
        latency.buffer_frames,
        ms(latency.buffer_frames),
        latency.stream_latency_frames,
        ms(latency.stream_latency_frames),
    );
    if let (Some(name), Some(format)) = (device.input_name(), device.input_sample_format()) {
        println!(
            "  input {name}: {format:?}, {} inputs",
            config.input_channels
        );
    }
}

#[cfg(not(windows))]
fn open_wasapi(
    _options: &RunOptions,
    _wanted: Option<&str>,
) -> Result<(AnyDevice, Vec<AnyDevice>), String> {
    Err(NO_DEVICES.to_owned())
}
