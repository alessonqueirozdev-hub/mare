//! What MARÉ needs from the operating system.
//!
//! Three things, each with one implementation per platform:
//!
//! - [`SharedMemory`]: a named segment both sides map (`CreateFileMappingW`
//!   on Windows, `shm_open` elsewhere);
//! - [`Doorbell`]: a counter in shared memory that one side adds to and the
//!   other waits on. The counter is the truth; the waiter sleeps until it
//!   changes, on a named auto-reset event on Windows and on the counter
//!   itself with a futex on Linux. Other systems (macOS among them) have no
//!   wake object yet and wait by polling the counter (spinning briefly, then
//!   sleeping in steps of 50 µs), which works but costs latency and some
//!   processor time;
//! - [`ProcessWatch`]: whether another process has ended, so that the hub
//!   frees a client that died.
//!
//! Object names follow [`names`], from a [`HubName`].
//!
//! Only [`Doorbell::ring`] and [`Doorbell::wait`] are meant for real-time
//! threads. Neither allocates or takes a lock; `ring` makes one system call
//! (`SetEvent` on Windows, `FUTEX_WAKE` on Linux) and none elsewhere, and
//! `wait` is the one blocking call the protocol allows.

use std::fmt;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

pub mod names;

pub use names::HubName;

/// The id of the calling process.
pub fn current_pid() -> u32 {
    std::process::id()
}

/// A mapped segment of named shared memory.
///
/// The mapping is 64-byte aligned (it starts on a page) and zero-filled when
/// created. Dropping it unmaps it; on systems where names outlive their
/// users (not Windows), the creator also removes the name.
pub struct SharedMemory {
    inner: imp::SharedMemory,
}

impl SharedMemory {
    /// Creates a segment of `size` bytes under `name`. Fails with
    /// [`io::ErrorKind::AlreadyExists`] if the name is taken.
    pub fn create(name: &str, size: usize) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty segment"));
        }
        imp::SharedMemory::create(name, size).map(|inner| Self { inner })
    }

    /// Maps the whole of the existing segment `name`.
    pub fn open(name: &str) -> io::Result<Self> {
        imp::SharedMemory::open(name).map(|inner| Self { inner })
    }

    /// Removes `name`, so that nobody else can open it. Mappings already made
    /// stay valid. On Windows a name lives exactly as long as its users, so
    /// this does nothing there.
    pub fn unlink(name: &str) -> io::Result<()> {
        imp::SharedMemory::unlink(name)
    }

    /// The first byte of the mapping.
    pub fn base(&self) -> NonNull<u8> {
        self.inner.base()
    }

    /// Bytes mapped. At least the size the segment was created with; it may
    /// be rounded up to a whole page.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the mapping is empty (it never is).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for SharedMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedMemory")
            .field("base", &self.base())
            .field("len", &self.len())
            .finish()
    }
}

// SAFETY: the mapping is plain memory owned by this value; sharing its
// address between threads is what it is for.
unsafe impl Send for SharedMemory {}
// SAFETY: as above.
unsafe impl Sync for SharedMemory {}

/// What a waiting thread keeps between waits.
///
/// On Windows a precise waiter owns a high-resolution waitable timer, so that
/// waits shorter than a millisecond end on time (a plain timed wait counts
/// in whole milliseconds). It is made once, off the real-time path.
pub struct Waiter {
    inner: imp::Waiter,
}

impl Waiter {
    /// A waiter whose timeouts are kept to well under a millisecond, for the
    /// hub's cycle thread.
    pub fn precise() -> io::Result<Self> {
        imp::Waiter::precise().map(|inner| Self { inner })
    }

    /// A waiter whose timeouts may be late by up to a scheduler tick, for
    /// waits whose timeout is only a fallback (a client checking that its hub
    /// is still there).
    pub fn coarse() -> Self {
        Self {
            inner: imp::Waiter::coarse(),
        }
    }
}

// SAFETY: a waiter is used by one thread at a time (`wait` takes it by
// `&mut`); the timer handle it may hold can move between threads.
unsafe impl Send for Waiter {}

impl fmt::Debug for Waiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiter").finish_non_exhaustive()
    }
}

/// A counter in shared memory, rung by one side and waited on by the other.
pub struct Doorbell {
    word: NonNull<AtomicU32>,
    wake: imp::Wake,
}

// SAFETY: the counter is an atomic and the wake object is a kernel handle,
// both usable from any thread.
unsafe impl Send for Doorbell {}
// SAFETY: as above.
unsafe impl Sync for Doorbell {}

impl Doorbell {
    /// The doorbell whose counter is `word`, creating its wake object under
    /// `name`.
    ///
    /// # Safety
    ///
    /// `word` must stay valid (mapped) for as long as the doorbell lives.
    pub unsafe fn create(word: NonNull<AtomicU32>, name: &str) -> io::Result<Self> {
        Ok(Self {
            word,
            wake: imp::Wake::create(name)?,
        })
    }

    /// The doorbell whose counter is `word`, opening the wake object its
    /// creator made under `name`.
    ///
    /// # Safety
    ///
    /// As for [`Doorbell::create`].
    pub unsafe fn open(word: NonNull<AtomicU32>, name: &str) -> io::Result<Self> {
        Ok(Self {
            word,
            wake: imp::Wake::open(name)?,
        })
    }

    fn counter(&self) -> &AtomicU32 {
        // SAFETY: the constructors require the word to stay valid.
        unsafe { self.word.as_ref() }
    }

    /// The counter's value now. Read it before checking for work, and pass it
    /// to [`Doorbell::wait`], so that a ring in between is not missed.
    pub fn seen(&self) -> u32 {
        self.counter().load(Ordering::Acquire)
    }

    /// Adds one to the counter (release) and wakes the waiter.
    pub fn ring(&self) {
        self.counter().fetch_add(1, Ordering::Release);
        self.wake.signal(self.counter());
    }

    /// Waits until the counter differs from `seen`, or `timeout` passes.
    /// Returns whether it changed.
    pub fn wait(&self, waiter: &mut Waiter, seen: u32, timeout: Duration) -> bool {
        imp::wait(&self.wake, self.counter(), seen, timeout, &mut waiter.inner)
    }
}

impl fmt::Debug for Doorbell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Doorbell")
            .field("count", &self.seen())
            .finish_non_exhaustive()
    }
}

/// Polls `word` until it differs from `seen` or `timeout` passes: the wait
/// of systems with no wake object.
#[cfg_attr(any(windows, target_os = "linux"), allow(dead_code))]
fn poll_wait(word: &AtomicU32, seen: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let mut spins = 0u32;
    loop {
        if word.load(Ordering::Acquire) != seen {
            return true;
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return false;
        }
        if spins < 64 {
            std::hint::spin_loop();
        } else if spins < 96 {
            std::thread::yield_now();
        } else {
            std::thread::sleep((timeout - elapsed).min(Duration::from_micros(50)));
        }
        spins = spins.saturating_add(1);
    }
}

/// A real-time thread's raised priority, for as long as the value lives.
///
/// Made by [`promote_current_thread`], and dropped on the same thread (it is
/// neither `Send` nor `Sync`).
pub struct RealtimeThread {
    inner: imp::RealtimeThread,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl RealtimeThread {
    /// Whether the system raised the thread's priority. Where MARÉ does not
    /// know how yet (not Windows), it did not.
    pub fn is_promoted(&self) -> bool {
        self.inner.is_promoted()
    }
}

impl fmt::Debug for RealtimeThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeThread")
            .field("promoted", &self.is_promoted())
            .finish()
    }
}

/// Raises the calling thread to the priority audio threads run at: on
/// Windows, registers it with the Multimedia Class Scheduler Service as a
/// "Pro Audio" task at high priority. Elsewhere it changes nothing yet, and
/// says so through [`RealtimeThread::is_promoted`].
///
/// Call it once, when the thread starts, not on the real-time path.
pub fn promote_current_thread() -> io::Result<RealtimeThread> {
    imp::RealtimeThread::promote().map(|inner| RealtimeThread {
        inner,
        _not_send: std::marker::PhantomData,
    })
}

/// Watches whether another process has ended.
pub struct ProcessWatch {
    inner: imp::ProcessWatch,
}

impl ProcessWatch {
    /// Starts watching process `pid`. Fails if there is no such process.
    pub fn open(pid: u32) -> io::Result<Self> {
        imp::ProcessWatch::open(pid).map(|inner| Self { inner })
    }

    /// Whether the process has ended. Does not block.
    ///
    /// On Windows the watch holds the process, so its id cannot be reused
    /// while the watch lives. Elsewhere the id is checked with `kill(pid, 0)`:
    /// a process that has ended but not been reaped by its parent still
    /// counts as running, and a reused id would hide the end.
    pub fn has_exited(&self) -> bool {
        self.inner.has_exited()
    }
}

// SAFETY: a process handle, or a process id, is usable from any thread.
unsafe impl Send for ProcessWatch {}
// SAFETY: as above; `has_exited` only queries.
unsafe impl Sync for ProcessWatch {}

impl fmt::Debug for ProcessWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessWatch").finish_non_exhaustive()
    }
}
