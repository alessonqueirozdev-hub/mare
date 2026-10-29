//! The hub's clock, which tests replace.
//!
//! The cycle thread asks a [`Clock`] two things: what time it is, and to wait
//! on the hub's doorbell until a deadline. [`SystemClock`] answers with the
//! system's monotonic clock and a real wait. [`FakeClock`] answers with a time
//! that only moves when told to, and its wait moves the time straight to the
//! deadline instead of sleeping, so that a test decides exactly which clients
//! finish in time: those that ran before the hub waited.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mare_sys::{Doorbell, Waiter};

/// Time, and waiting, as the hub sees them.
pub trait Clock: Send + Sync {
    /// Time since an origin of the clock's choosing.
    fn now(&self) -> Duration;

    /// Waits until `bell` differs from `seen` or the clock reaches
    /// `deadline`. Returns whether the bell changed.
    fn wait_bell(
        &self,
        bell: &Doorbell,
        waiter: &mut Waiter,
        seen: u32,
        deadline: Duration,
    ) -> bool;
}

/// The system's monotonic clock.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose origin is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn wait_bell(
        &self,
        bell: &Doorbell,
        waiter: &mut Waiter,
        seen: u32,
        deadline: Duration,
    ) -> bool {
        let timeout = deadline.saturating_sub(self.now());
        bell.wait(waiter, seen, timeout)
    }
}

/// A clock that moves only when told to.
#[derive(Default)]
pub struct FakeClock {
    nanos: AtomicU64,
}

impl FakeClock {
    /// A clock at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock to `time`, unless it is already later.
    pub fn advance_to(&self, time: Duration) {
        self.nanos
            .fetch_max(time.as_nanos() as u64, Ordering::AcqRel);
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.nanos.fetch_add(by.as_nanos() as u64, Ordering::AcqRel);
    }
}

impl fmt::Debug for FakeClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeClock")
            .field("now", &self.now())
            .finish()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Acquire))
    }

    fn wait_bell(
        &self,
        bell: &Doorbell,
        _waiter: &mut Waiter,
        seen: u32,
        deadline: Duration,
    ) -> bool {
        if bell.seen() != seen {
            return true;
        }
        // Nobody can ring while the test's single thread is here, so the
        // wait would last until the deadline.
        self.advance_to(deadline);
        false
    }
}
