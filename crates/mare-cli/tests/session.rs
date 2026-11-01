//! A session: a hub on a device with followers joined to it, as `mare run`
//! makes one. The devices are fake, in real time.

use std::time::{Duration, Instant};

use mare_cli::{AnyDevice, Session, StartError};
use mare_client::{Client, ClientConfig};
use mare_hub::fake::{FakeDevice, Pacing};
use mare_hub::{DeviceConfig, HubConfig};
use mare_sys::{HubName, current_pid};

fn device(rate: u32, period: u32, skew: f64) -> AnyDevice {
    Box::new(
        FakeDevice::new(
            DeviceConfig {
                sample_rate: rate,
                period_frames: period,
                max_frames: period,
                input_channels: 2,
                output_channels: 2,
            },
            Pacing::RealTime,
        )
        .with_skew(skew),
    )
}

#[test]
fn followers_add_their_channels_and_report_their_links() {
    let hub = HubName::new(&format!("f{:x}", current_pid() & 0xF_FFFF)).unwrap();
    let session = Session::start(
        &HubConfig::new(hub.clone()),
        device(48_000, 64, 0.0),
        vec![device(48_000, 96, 300e-6), device(48_000, 128, -200e-6)],
    )
    .unwrap();
    assert_eq!(session.followers(), 2);
    let info = mare_client::probe(&hub).unwrap();
    assert_eq!((info.device_inputs, info.device_outputs), (6, 6));

    let mut client = Client::connect(&hub, &ClientConfig::new("test", 6, 6)).unwrap();
    let start = Instant::now();
    let mut cycles = 0;
    while cycles < 300 {
        assert!(start.elapsed() < Duration::from_secs(30), "too slow");
        if client
            .wait_cycle(Duration::from_secs(5), |cycle| {
                for channel in 0..6 {
                    cycle.output(channel).unwrap().fill(0.25);
                }
            })
            .unwrap()
        {
            cycles += 1;
        }
    }
    client.leave();
    let mut session = session;
    assert!(session.newly_stopped_followers().is_empty());
    assert!(!session.has_stopped());

    let report = session.stop();
    assert!(report.error.is_none());
    assert!(report.stats.client_cycles >= 300);
    assert_eq!(report.followers.len(), 2);
    for (index, follower) in report.followers.iter().enumerate() {
        assert!(follower.error.is_none(), "follower {index}");
        let capture = follower.capture.expect("the follower records");
        let playback = follower.playback.expect("the follower plays");
        assert!(capture.frames_out > 0 && playback.frames_out > 0);
        assert!(follower.capture_latency.unwrap() > 0.0);
        assert!(follower.playback_latency.unwrap() > 0.0);
    }
}

#[test]
fn a_follower_at_another_rate_is_refused() {
    let hub = HubName::new(&format!("r{:x}", current_pid() & 0xF_FFFF)).unwrap();
    let error = Session::start(
        &HubConfig::new(hub.clone()),
        device(48_000, 64, 0.0),
        vec![device(44_100, 64, 0.0)],
    )
    .unwrap_err();
    assert!(matches!(error, StartError::Rate(_)), "{error:?}");
    assert!(error.to_string().contains("--exclusive --rate"));
    // Nothing was left running under the name.
    assert!(mare_client::probe(&hub).is_err());
}
