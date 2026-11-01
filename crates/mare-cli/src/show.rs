//! What the program prints.

use std::time::Duration;

use mare_client::HubInfo;
use mare_hub::{LinkStats, Stats};
use mare_sys::HubName;
use mare_wasapi::Endpoint;

/// `frames` at `rate`, in milliseconds.
pub fn ms(frames: f64, rate: u32) -> f64 {
    frames * 1000.0 / f64::from(rate)
}

/// The clients connected now.
pub fn connected(stats: &Stats) -> u64 {
    stats
        .accepted
        .saturating_sub(stats.left)
        .saturating_sub(stats.died)
}

/// The line that shows a running hub, refreshed every second.
pub fn status_line(stats: &Stats, running: Duration) -> String {
    let seconds = running.as_secs();
    format!(
        "{:02}:{:02}:{:02}  clients {}  cycles {}  late {}  busy {}  longest wait {:.2} ms",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        connected(stats),
        stats.cycles,
        stats.late,
        stats.busy,
        stats.longest_wait_ns as f64 / 1e6
    )
}

/// What changed in the clients between two readings, one line each.
pub fn client_changes(before: &Stats, now: &Stats) -> Vec<String> {
    let mut lines = Vec::new();
    let mut say = |count: u64, what: &str| {
        if count > 0 {
            let clients = if count == 1 { "client" } else { "clients" };
            lines.push(format!(
                "{count} {clients} {what} ({} connected)",
                connected(now)
            ));
        }
    };
    say(now.accepted - before.accepted, "joined");
    say(now.left - before.left, "left");
    say(now.died - before.died, "ended without leaving");
    say(now.rejected - before.rejected, "refused");
    lines
}

/// A running hub, as `mare probe` shows it.
pub fn hub_info(hub: &HubName, info: &HubInfo) -> String {
    let channels = |n: u32| {
        if n == 0 {
            "unknown".to_owned()
        } else {
            n.to_string()
        }
    };
    format!(
        "hub {}: {} Hz, period {} frames ({:.2} ms), at most {} frames, \
         device inputs {}, outputs {}, process {}",
        hub.as_str(),
        info.sample_rate,
        info.period_frames,
        ms(f64::from(info.period_frames), info.sample_rate),
        info.max_frames,
        channels(info.device_inputs),
        channels(info.device_outputs),
        info.hub_pid
    )
}

/// What a link between a follower and the hub did.
pub fn link(stats: &LinkStats, latency: Option<f64>, rate: u32) -> String {
    let mut line = format!(
        "drift {:+.1} ppm, silence while starting {} frames, starved {}, overflowed {}",
        stats.drift * 1e6,
        stats.priming,
        stats.starved,
        stats.overflowed
    );
    if let Some(frames) = latency {
        line += &format!(", added latency {:.2} ms", ms(frames, rate));
    }
    line
}

/// An endpoint as `mare devices` lists it.
pub fn endpoint(endpoint: &Endpoint) -> String {
    let default = if endpoint.is_default {
        "  (default)"
    } else {
        ""
    };
    format!("  {}{default}\n      {}", endpoint.name, endpoint.id)
}

/// The endpoint `wanted` names: the one with that id, else the one with that
/// name (ignoring case), else the only one whose name contains it.
pub fn find_endpoint<'a>(endpoints: &'a [Endpoint], wanted: &str) -> Result<&'a Endpoint, String> {
    if let Some(e) = endpoints.iter().find(|e| e.id == wanted) {
        return Ok(e);
    }
    let lower = wanted.to_lowercase();
    if let Some(e) = endpoints.iter().find(|e| e.name.to_lowercase() == lower) {
        return Ok(e);
    }
    let matching: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&lower))
        .collect();
    match matching.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no device is named like {wanted:?} (mare devices lists them)"
        )),
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.name.as_str()).collect();
            Err(format!(
                "{wanted:?} names {} devices: {}; give more of the name, or the id",
                many.len(),
                names.join("; ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str, name: &str) -> Endpoint {
        Endpoint {
            id: id.into(),
            name: name.into(),
            is_default: false,
        }
    }

    #[test]
    fn a_device_is_found_by_id_name_or_a_part_of_it_that_is_unique() {
        let list = [
            endpoint("{1}", "Speakers (Realtek(R) Audio)"),
            endpoint("{2}", "Line (UMC404HD 192k)"),
            endpoint("{3}", "Speakers (UMC404HD 192k)"),
        ];
        assert_eq!(find_endpoint(&list, "{2}").unwrap().id, "{2}");
        assert_eq!(find_endpoint(&list, "realtek").unwrap().id, "{1}");
        assert_eq!(
            find_endpoint(&list, "speakers (umc404hd 192k)").unwrap().id,
            "{3}"
        );
        let ambiguous = find_endpoint(&list, "UMC").unwrap_err();
        assert!(ambiguous.contains("2 devices"), "{ambiguous}");
        assert!(find_endpoint(&list, "Focusrite").is_err());
    }

    #[test]
    fn the_status_line_counts_the_clients_connected_now() {
        // The counters cannot be built field by field outside the hub.
        let mut stats = Stats::default();
        stats.cycles = 3000;
        stats.accepted = 3;
        stats.left = 1;
        stats.died = 1;
        stats.late = 2;
        stats.longest_wait_ns = 420_000;
        assert_eq!(
            status_line(&stats, Duration::from_secs(3725)),
            "01:02:05  clients 1  cycles 3000  late 2  busy 0  longest wait 0.42 ms"
        );
        let before = Stats::default();
        assert_eq!(
            client_changes(&before, &stats),
            [
                "3 clients joined (1 connected)",
                "1 client left (1 connected)",
                "1 client ended without leaving (1 connected)"
            ]
        );
    }
}
