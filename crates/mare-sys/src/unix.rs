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

/// No wake object: waiting polls the counter.
pub(crate) struct Wake;

impl Wake {
    pub(crate) fn create(_name: &str) -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn open(_name: &str) -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn signal(&self) {}
}

pub(crate) struct Waiter;

impl Waiter {
    pub(crate) fn precise() -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn coarse() -> Self {
        Self
    }
}

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
}

impl ProcessWatch {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let watch = Self {
            pid: libc::pid_t::try_from(pid)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?,
        };
        if watch.has_exited() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such process"));
        }
        Ok(watch)
    }

    pub(crate) fn has_exited(&self) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(self.pid, 0) } == 0 {
            return false;
        }
        // EPERM means it exists but belongs to someone else.
        io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}
