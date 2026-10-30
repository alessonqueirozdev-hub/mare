//! Linux, macOS and other Unix systems: POSIX shared memory, doorbells that
//! poll their counter, and `kill(pid, 0)`.

use std::ffi::CString;
use std::io;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::AtomicU32;
use std::time::Duration;

// `shm_open` is variadic on Apple systems, where the mode is promoted to an
// unsigned int; elsewhere it takes a `mode_t`.
#[cfg(target_vendor = "apple")]
const OWNER_ONLY: libc::c_uint = 0o600;
#[cfg(not(target_vendor = "apple"))]
const OWNER_ONLY: libc::mode_t = 0o600;

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))
}

pub(crate) struct SharedMemory {
    base: NonNull<u8>,
    len: usize,
    /// The name, removed on drop by the segment's creator.
    owned_name: Option<CString>,
}

impl SharedMemory {
    pub(crate) fn create(name: &str, size: usize) -> io::Result<Self> {
        let name = c_name(name)?;
        // SAFETY: plain call with a NUL-terminated name.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                OWNER_ONLY,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is the descriptor just opened.
        let result = if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Self::map(fd, size)
        };
        // SAFETY: `fd` is open; the mapping does not need it.
        unsafe { libc::close(fd) };
        match result {
            Ok(mut memory) => {
                memory.owned_name = Some(name);
                Ok(memory)
            }
            Err(error) => {
                // SAFETY: plain call; the name was created above.
                unsafe { libc::shm_unlink(name.as_ptr()) };
                Err(error)
            }
        }
    }

    pub(crate) fn open(name: &str) -> io::Result<Self> {
        let name = c_name(name)?;
        // SAFETY: plain call with a NUL-terminated name.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed is a valid `stat`, and `fd` is open.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let result = if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            Err(io::Error::last_os_error())
        } else if stat.st_size <= 0 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the segment has no size yet",
            ))
        } else {
            Self::map(fd, stat.st_size as usize)
        };
        // SAFETY: `fd` is open; the mapping does not need it.
        unsafe { libc::close(fd) };
        result
    }

    fn map(fd: libc::c_int, len: usize) -> io::Result<Self> {
        // SAFETY: plain call; the kernel picks the address.
        let ptr = unsafe {
            libc::mmap(
                null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            base: NonNull::new(ptr.cast()).expect("mmap returned null"),
            len,
            owned_name: None,
        })
    }

    pub(crate) fn unlink(name: &str) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: plain call with a NUL-terminated name.
        if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn base(&self) -> NonNull<u8> {
        self.base
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // SAFETY: the mapping was made by `map` and is unmapped once.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
        if let Some(name) = &self.owned_name {
            // SAFETY: plain call; a name already removed is not an error here.
            unsafe { libc::shm_unlink(name.as_ptr()) };
        }
    }
}

/// No named object: the waiter sleeps on the counter itself, with a futex on
/// Linux and with `os_sync_wait_on_address` on macOS 14.4 and later (looked
/// up when the doorbell is made, so older systems still run); elsewhere it
/// polls the counter.
pub(crate) struct Wake {
    #[cfg(target_os = "macos")]
    sync: Option<macos::OsSync>,
}

impl Wake {
    pub(crate) fn create(_name: &str) -> io::Result<Self> {
        Ok(Self::new())
    }

    pub(crate) fn open(_name: &str) -> io::Result<Self> {
        Ok(Self::new())
    }

    fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            sync: macos::OsSync::find(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn signal(&self, word: &AtomicU32) {
        if let Some(sync) = self.sync {
            sync.wake_all(word);
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn signal(&self, word: &AtomicU32) {
        // Not FUTEX_PRIVATE: the waiter may be in another process.
        // SAFETY: `word` is a valid, aligned 32-bit atomic; waking has no
        // other effect.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE,
                i32::MAX,
                std::ptr::null::<libc::timespec>(),
                std::ptr::null::<u32>(),
                0,
            );
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn signal(&self, _word: &AtomicU32) {}
}

#[cfg(target_os = "macos")]
mod macos {
    //! `os_sync_wait_on_address` and `os_sync_wake_by_address_all`, Apple's
    //! public wait-on-address calls (macOS 14.4), with the flag that makes
    //! them work across processes on shared memory.

    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    type WaitFn = unsafe extern "C" fn(
        addr: *mut libc::c_void,
        value: u64,
        size: usize,
        flags: u32,
        clock: u32,
        timeout_ns: u64,
    ) -> libc::c_int;
    type WakeFn =
        unsafe extern "C" fn(addr: *mut libc::c_void, size: usize, flags: u32) -> libc::c_int;

    /// `OS_SYNC_WAIT_ON_ADDRESS_SHARED` and `OS_SYNC_WAKE_BY_ADDRESS_SHARED`.
    const SHARED: u32 = 1;
    /// `OS_CLOCK_MACH_ABSOLUTE_TIME`.
    const MACH_ABSOLUTE_TIME: u32 = 32;
    /// `RTLD_DEFAULT`: look in every image loaded.
    const DEFAULT: *mut libc::c_void = -2isize as *mut libc::c_void;

    #[derive(Clone, Copy)]
    pub(super) struct OsSync {
        wait: WaitFn,
        wake: WakeFn,
    }

    impl OsSync {
        /// The two calls, if this system has them.
        pub(super) fn find() -> Option<Self> {
            // SAFETY: dlsym with NUL-terminated names; a non-null result is
            // the function with the documented signature.
            unsafe {
                let wait = libc::dlsym(DEFAULT, c"os_sync_wait_on_address_with_timeout".as_ptr());
                let wake = libc::dlsym(DEFAULT, c"os_sync_wake_by_address_all".as_ptr());
                if wait.is_null() || wake.is_null() {
                    return None;
                }
                Some(Self {
                    wait: std::mem::transmute::<*mut libc::c_void, WaitFn>(wait),
                    wake: std::mem::transmute::<*mut libc::c_void, WakeFn>(wake),
                })
            }
        }

        pub(super) fn wake_all(self, word: &AtomicU32) {
            // SAFETY: a valid, aligned 4-byte word; waking has no other
            // effect, and no waiter is not an error worth reporting.
            unsafe { (self.wake)(word.as_ptr().cast(), 4, SHARED) };
        }

        /// Sleeps while `word` holds `seen`, for at most `timeout`.
        pub(super) fn wait(self, word: &AtomicU32, seen: u32, timeout: Duration) {
            // Zero would mean no timeout at all.
            let nanos = (timeout.as_nanos() as u64).max(1_000);
            // SAFETY: as above. Returns at once if the word no longer holds
            // `seen`; a timeout, an interruption or a spurious wake are all
            // rechecked by the caller.
            unsafe {
                (self.wait)(
                    word.as_ptr().cast(),
                    u64::from(seen),
                    4,
                    SHARED,
                    MACH_ABSOLUTE_TIME,
                    nanos,
                )
            };
        }
    }
}

pub(crate) struct Waiter;

impl Waiter {
    pub(crate) fn precise() -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn coarse() -> Self {
        Self
    }

    /// No wait here can also end on a process: futexes and polling see only
    /// the counter. The caller still learns of the end at its timeout.
    pub(crate) fn watching(self, _process: ProcessWatch) -> Self {
        self
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn wait(
    _wake: &Wake,
    word: &AtomicU32,
    seen: u32,
    timeout: Duration,
    _waiter: &mut Waiter,
) -> bool {
    use std::sync::atomic::Ordering;
    let start = std::time::Instant::now();
    loop {
        if word.load(Ordering::Acquire) != seen {
            return true;
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return false;
        }
        let remaining = timeout - elapsed;
        let relative = libc::timespec {
            tv_sec: remaining.as_secs() as libc::time_t,
            tv_nsec: libc::c_long::from(remaining.subsec_nanos() as i32),
        };
        // Sleeps only while the counter still holds `seen`; returns at once
        // (EAGAIN) if it changed in between, which the loop then sees. A
        // timeout, an interruption or a spurious wake are all rechecked.
        // SAFETY: `word` is a valid, aligned 32-bit atomic; `relative` lives
        // across the call.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAIT,
                seen,
                &relative as *const libc::timespec,
                std::ptr::null::<u32>(),
                0,
            );
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn wait(
    wake: &Wake,
    word: &AtomicU32,
    seen: u32,
    timeout: Duration,
    _waiter: &mut Waiter,
) -> bool {
    use std::sync::atomic::Ordering;
    let Some(sync) = wake.sync else {
        return crate::poll_wait(word, seen, timeout);
    };
    let start = std::time::Instant::now();
    loop {
        if word.load(Ordering::Acquire) != seen {
            return true;
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return false;
        }
        sync.wait(word, seen, timeout - elapsed);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn wait(
    _wake: &Wake,
    word: &AtomicU32,
    seen: u32,
    timeout: Duration,
    _waiter: &mut Waiter,
) -> bool {
    crate::poll_wait(word, seen, timeout)
}

pub(crate) struct ProcessWatch {
    pid: libc::pid_t,
    /// On Linux 5.3 and later, a descriptor of the process itself, which
    /// becomes readable when it ends, whether or not its parent has reaped
    /// it, and which a reused id cannot fool. Elsewhere, or on older
    /// kernels, -1, and the id is checked with `kill(pid, 0)`.
    pidfd: libc::c_int,
}

impl ProcessWatch {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let pid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
        let watch = Self {
            pid,
            pidfd: open_pidfd(pid),
        };
        if watch.has_exited() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such process"));
        }
        Ok(watch)
    }

    pub(crate) fn has_exited(&self) -> bool {
        if self.pidfd >= 0 {
            let mut poll = libc::pollfd {
                fd: self.pidfd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; a zero timeout never blocks.
            let ready = unsafe { libc::poll(&mut poll, 1, 0) };
            return ready > 0 && poll.revents & libc::POLLIN != 0;
        }
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(self.pid, 0) } == 0 {
            return false;
        }
        // EPERM means it exists but belongs to someone else.
        io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    pub(crate) fn exact(&self) -> bool {
        self.pidfd >= 0
    }
}

impl Drop for ProcessWatch {
    fn drop(&mut self) {
        if self.pidfd >= 0 {
            // SAFETY: the descriptor is open and owned by this value.
            unsafe { libc::close(self.pidfd) };
        }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: libc::pid_t) -> libc::c_int {
    // SAFETY: plain system call; failure (an old kernel, no such process)
    // returns -1, and the caller falls back.
    unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int }
}

#[cfg(not(target_os = "linux"))]
fn open_pidfd(_pid: libc::pid_t) -> libc::c_int {
    -1
}

/// On Linux, the thread is moved to `SCHED_FIFO`, at priority 70 or the
/// most the user's `RLIMIT_RTPRIO` allows; an ordinary user with no
/// real-time limit is refused, which is reported, not an error. On macOS it
/// is given a Mach time-constraint policy, which any user may ask for.
/// Elsewhere nothing changes.
pub(crate) struct RealtimeThread {
    promoted: bool,
    #[cfg(target_os = "linux")]
    previous: (libc::c_int, libc::sched_param),
}

impl RealtimeThread {
    #[cfg(target_os = "linux")]
    pub(crate) fn promote() -> io::Result<Self> {
        /// The priority asked for: high, below the kernel's own threads and
        /// what JACK-style audio systems give their clients' servers.
        const WANTED: libc::c_int = 70;
        // SAFETY: plain calls on the calling thread with valid out-pointers.
        unsafe {
            let thread = libc::pthread_self();
            let mut policy = 0;
            let mut previous: libc::sched_param = std::mem::zeroed();
            if libc::pthread_getschedparam(thread, &mut policy, &mut previous) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut limit: libc::rlimit = std::mem::zeroed();
            let allowed = if libc::getrlimit(libc::RLIMIT_RTPRIO, &mut limit) == 0
                && limit.rlim_cur != libc::RLIM_INFINITY
            {
                (limit.rlim_cur as libc::c_int).min(WANTED)
            } else {
                WANTED
            };
            let mut promoted = false;
            if allowed > 0 {
                let mut param: libc::sched_param = std::mem::zeroed();
                param.sched_priority = allowed.min(libc::sched_get_priority_max(libc::SCHED_FIFO));
                promoted = libc::pthread_setschedparam(thread, libc::SCHED_FIFO, &param) == 0;
            }
            // With no limit set, root may still succeed at the wanted value.
            if !promoted && allowed == 0 {
                let mut param: libc::sched_param = std::mem::zeroed();
                param.sched_priority = WANTED;
                promoted = libc::pthread_setschedparam(thread, libc::SCHED_FIFO, &param) == 0;
            }
            Ok(Self {
                promoted,
                previous: (policy, previous),
            })
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn promote() -> io::Result<Self> {
        // SAFETY: plain calls on the calling thread with valid arguments.
        unsafe {
            let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
            if libc::mach_timebase_info(&mut timebase) != libc::KERN_SUCCESS || timebase.numer == 0
            {
                return Ok(Self { promoted: false });
            }
            let ticks =
                |nanos: u64| (nanos * u64::from(timebase.denom) / u64::from(timebase.numer)) as u32;
            // An audio thread woken every few milliseconds that needs about
            // a millisecond and a half, within three.
            let mut policy = libc::thread_time_constraint_policy {
                period: ticks(5_000_000),
                computation: ticks(1_500_000),
                constraint: ticks(3_000_000),
                preemptible: 1,
            };
            let thread = libc::pthread_mach_thread_np(libc::pthread_self());
            let result = libc::thread_policy_set(
                thread,
                libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
                (&mut policy as *mut libc::thread_time_constraint_policy).cast(),
                libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            );
            Ok(Self {
                promoted: result == libc::KERN_SUCCESS,
            })
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn promote() -> io::Result<Self> {
        Ok(Self { promoted: false })
    }

    pub(crate) fn is_promoted(&self) -> bool {
        self.promoted
    }
}

impl Drop for RealtimeThread {
    fn drop(&mut self) {
        if !self.promoted {
            return;
        }
        #[cfg(target_os = "linux")]
        // SAFETY: restores the policy read from this same thread, which the
        // guard cannot leave.
        unsafe {
            libc::pthread_setschedparam(libc::pthread_self(), self.previous.0, &self.previous.1);
        }
        #[cfg(target_os = "macos")]
        // SAFETY: as above; the standard policy takes no data.
        unsafe {
            libc::thread_policy_set(
                libc::pthread_mach_thread_np(libc::pthread_self()),
                libc::THREAD_STANDARD_POLICY as libc::thread_policy_flavor_t,
                std::ptr::null_mut(),
                0,
            );
        }
    }
}
