//! A hub running on a device and its followers, until it is stopped.

use std::fmt;
use std::io;
use std::sync::Arc;

use mare_hub::{
    Aggregate, Clock, Device, DeviceError, DeviceLatency, FollowerThread, HubConfig, HubError,
    LatencySource, LinkStats, RateMismatch, RunningHub, Stats, SystemClock, aggregate,
};

/// A device of any kind.
pub type AnyDevice = Box<dyn Device>;

/// Why a hub could not start.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartError {
    /// A follower runs at another rate than the master.
    Rate(RateMismatch),
    /// The hub refused.
    Hub(HubError),
    /// A follower's thread could not be made.
    Thread(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rate(e) => write!(f, "{e}; set one rate for all with --exclusive --rate"),
            Self::Hub(HubError::Exists) => write!(f, "a hub of that name is running already"),
            Self::Hub(e) => write!(f, "{e}"),
            Self::Thread(e) => write!(f, "starting a follower: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// A hub running on its threads, one per device.
pub struct Session {
    hub: RunningHub<Aggregate<AnyDevice>>,
    followers: Vec<FollowerThread<AnyDevice>>,
    /// Which followers have been seen to stop.
    reported: Vec<bool>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("hub", &self.hub)
            .field("followers", &self.followers.len())
            .finish()
    }
}

/// What a follower did, once stopped.
#[derive(Debug)]
pub struct FollowerReport {
    /// The error that stopped its device, if one did.
    pub error: Option<DeviceError>,
    /// What the hub took from its inputs; `None` if it has none.
    pub capture: Option<LinkStats>,
    /// What it took from the hub for its outputs; `None` if it has none.
    pub playback: Option<LinkStats>,
    /// The latency resampling added to its inputs, in frames.
    pub capture_latency: Option<f64>,
    /// The latency resampling added to its outputs, in frames.
    pub playback_latency: Option<f64>,
}

/// What a session did, once stopped.
#[derive(Debug)]
pub struct Report {
    /// The hub's counters.
    pub stats: Stats,
    /// The error that stopped the master device, if one did.
    pub error: Option<DeviceError>,
    /// Each follower's, in order.
    pub followers: Vec<FollowerReport>,
}

impl Session {
    /// Starts a hub on `master`, with `followers` joined to it, timed by the
    /// system clock.
    pub fn start(
        config: &HubConfig,
        master: AnyDevice,
        followers: Vec<AnyDevice>,
    ) -> Result<Self, StartError> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let (device, followers) =
            aggregate(master, followers, Arc::clone(&clock)).map_err(StartError::Rate)?;
        let hub = mare_hub::start(config, device, clock).map_err(StartError::Hub)?;
        let followers = followers
            .into_iter()
            .map(|f| f.spawn())
            .collect::<Result<Vec<_>, _>>()
            .map_err(StartError::Thread)?;
        let reported = vec![false; followers.len()];
        Ok(Self {
            hub,
            followers,
            reported,
        })
    }

    /// The hub's counters.
    pub fn stats(&self) -> Stats {
        self.hub.stats()
    }

    /// The latency published to programs, and where it comes from.
    pub fn latency(&self) -> (DeviceLatency, LatencySource) {
        self.hub.latency()
    }

    /// Publishes another latency to programs: a measured one, say.
    pub fn set_latency(&self, latency: DeviceLatency, source: LatencySource) {
        self.hub.set_latency(latency, source);
    }

    /// Whether the hub's cycle thread runs at real-time priority.
    pub fn is_realtime(&self) -> bool {
        self.hub.is_realtime()
    }

    /// How many followers' threads run at real-time priority.
    pub fn realtime_followers(&self) -> usize {
        self.followers.iter().filter(|f| f.is_realtime()).count()
    }

    /// How many followers there are.
    pub fn followers(&self) -> usize {
        self.followers.len()
    }

    /// Whether the master device has stopped, which stops the hub.
    pub fn has_stopped(&self) -> bool {
        self.hub.has_stopped()
    }

    /// The followers that have stopped since this was last asked. The hub
    /// carries on without them: what they recorded turns to silence, and what
    /// they played is dropped.
    pub fn newly_stopped_followers(&mut self) -> Vec<usize> {
        let mut stopped = Vec::new();
        for (index, follower) in self.followers.iter().enumerate() {
            if !self.reported[index] && follower.has_stopped() {
                self.reported[index] = true;
                stopped.push(index);
            }
        }
        stopped
    }

    /// Stops the hub, then the followers, and says what they did.
    pub fn stop(self) -> Report {
        let (engine, error) = self.hub.stop();
        let stats = engine.stats();
        let device = engine.device();
        let followers = self
            .followers
            .into_iter()
            .enumerate()
            .map(|(index, thread)| {
                let (follower, error) = thread.stop();
                FollowerReport {
                    error,
                    capture: device.capture_stats(index),
                    playback: follower.playback_stats(),
                    capture_latency: device.capture_latency(index),
                    playback_latency: follower.playback_latency(),
                }
            })
            .collect();
        Report {
            stats,
            error,
            followers,
        }
    }
}
