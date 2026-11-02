//! The `mare` program itself, run as a user would run it. Only its null
//! device runs a hub here: no test opens a sound card.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use mare_client::{Client, ClientConfig};
use mare_sys::{HubName, current_pid};

fn mare(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mare"))
        .args(args)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A hub name no other test, or test run, uses at the same time.
fn unique(tag: char) -> HubName {
    HubName::new(&format!("{tag}{:x}", current_pid() & 0xF_FFFF)).unwrap()
}

/// Waits for the hub called `hub` to run.
fn wait_for(hub: &HubName) {
    let start = Instant::now();
    while mare_client::probe(hub).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "hub {} never ran",
            hub.as_str()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn help_and_version_and_nonsense() {
    let help = mare(&["help"]);
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("mare run [options]"));

    let version = mare(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        text(&version.stdout).trim(),
        format!("mare {}", env!("CARGO_PKG_VERSION"))
    );

    let nonsense = mare(&["run", "--period", "zero"]);
    assert_eq!(nonsense.status.code(), Some(2));
    assert!(text(&nonsense.stderr).contains("--period"));
}

#[test]
fn probing_a_hub_that_is_not_running_fails() {
    let hub = unique('p');
    let out = mare(&["probe", "--name", hub.as_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no hub called"), "{out:?}");
}

#[test]
fn a_null_hub_runs_a_client_until_its_time_is_up() {
    let hub = unique('n');
    let child = Command::new(env!("CARGO_BIN_EXE_mare"))
        .args([
            "run",
            "--null",
            "--name",
            hub.as_str(),
            "--period",
            "128",
            "--inputs",
            "1",
            "--outputs",
            "4",
            "--seconds",
            "6",
            "--input-latency",
            "290",
            "--output-latency",
            "310",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&hub);

    let probe = mare(&["probe", "--name", hub.as_str()]);
    assert!(probe.status.success(), "{probe:?}");
    let shown = text(&probe.stdout);
    assert!(
        shown.contains("48000 Hz, period 128 frames (2.67 ms)")
            && shown.contains("device inputs 1, outputs 4")
            && shown.contains(
                "latency: input 290 frames (6.04 ms), output 310 frames (6.46 ms); measured"
            ),
        "{shown}"
    );

    // A second hub of the same name is refused.
    let twin = mare(&["run", "--null", "--name", hub.as_str(), "--seconds", "1"]);
    assert_eq!(twin.status.code(), Some(1));
    assert!(text(&twin.stderr).contains("running already"), "{twin:?}");

    let mut client = Client::connect(&hub, &ClientConfig::new("test", 1, 4)).unwrap();
    assert_eq!(client.info().device_outputs, 4);
    assert_eq!(client.latency().output_frames, 310);
    let mut cycles = 0;
    while cycles < 100 {
        if client
            .wait_cycle(Duration::from_secs(5), |cycle| {
                assert_eq!(cycle.frames(), 128);
                cycle.output(0).unwrap().fill(0.5);
            })
            .unwrap()
        {
            cycles += 1;
        }
    }
    client.leave();

    let out = child.wait_with_output().unwrap();
    let shown = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    for line in [
        "null device: silence in, nothing out",
        &format!(
            "hub {} running: 48000 Hz, period 128 frames (2.67 ms), 1 inputs, 4 outputs",
            hub.as_str()
        ),
        "1 client joined (1 connected)",
        "1 client left (0 connected)",
        "stopping (time is up)",
        "clients: 1 joined, 1 left, 0 ended without leaving, 0 refused",
    ] {
        assert!(shown.contains(line), "{line:?} missing from:\n{shown}");
    }
    // The hub's name is free again.
    assert!(mare_client::probe(&hub).is_err());
}

#[test]
fn a_measured_latency_needs_both_figures_when_the_hub_has_inputs() {
    let hub = unique('m');
    let out = mare(&[
        "run",
        "--null",
        "--name",
        hub.as_str(),
        "--output-latency",
        "300",
        "--seconds",
        "1",
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("--input-latency"), "{out:?}");
    // The hub it started for a moment is gone again.
    assert!(mare_client::probe(&hub).is_err());

    // Without inputs, the output's is enough; a null device says nothing
    // of its own, so before it the latency is unknown.
    let quiet = mare(&["run", "--null", "--name", hub.as_str(), "--seconds", "0.3"]);
    assert!(
        text(&quiet.stdout).contains("latency: input unknown, output unknown; nobody has said")
    );
    let out = mare(&[
        "run",
        "--null",
        "--inputs",
        "0",
        "--name",
        hub.as_str(),
        "--output-latency",
        "300",
        "--seconds",
        "0.3",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out.stdout).contains("latency: input unknown, output 300 frames (6.25 ms); measured"),
        "{out:?}"
    );
}
