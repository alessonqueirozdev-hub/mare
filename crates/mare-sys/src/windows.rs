//! Windows: file mappings backed by the paging file, auto-reset events, and
//! process handles.

use std::io;
use std::ptr::{NonNull, null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW,
    EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS,
    WaitForMultipleObjects, WaitForSingleObject,
};

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(Some(0)).collect()
}

/// A handle closed on drop.
struct Handle(HANDLE);

impl Handle {
    fn new(raw: HANDLE) -> io::Result<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned by this value.
        unsafe { CloseHandle(self.0) };
    }
}

pub(crate) struct SharedMemory {
    _mapping: Handle,
    base: NonNull<u8>,
    len: usize,
}

impl SharedMemory {
    pub(crate) fn create(name: &str, size: usize) -> io::Result<Self> {
        let name = wide(name);
        let size = size as u64;
        // SAFETY: plain call; the name is NUL-terminated and outlives it.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                name.as_ptr(),
            )
        };
        // Read the error before anything else can change it.
        // SAFETY: plain call.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mapping = Handle::new(raw)?;
        if existed {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a segment with this name exists",
            ));
        }
        Self::map(mapping)
    }

    pub(crate) fn open(name: &str) -> io::Result<Self> {
        let name = wide(name);
        // SAFETY: plain call; the name is NUL-terminated and outlives it.
        let raw = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr()) };
        Self::map(Handle::new(raw)?)
    }

    fn map(mapping: Handle) -> io::Result<Self> {
        // SAFETY: the handle is a file mapping; zero length maps all of it.
        let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        let base = NonNull::new(view.Value.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: zeroed is a valid MEMORY_BASIC_INFORMATION.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable and of the size passed.
        let written = unsafe {
            VirtualQuery(
                base.as_ptr().cast(),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: the view was mapped above.
            unsafe { UnmapViewOfFile(view) };
            return Err(error);
        }
        Ok(Self {
            _mapping: mapping,
            base,
            len: info.RegionSize,
        })
    }

    pub(crate) fn unlink(_name: &str) -> io::Result<()> {
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
        // SAFETY: the view was mapped by `map` and is unmapped once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.as_ptr().cast(),
            })
        };
    }
}

pub(crate) struct Wake {
    event: Handle,
}

impl Wake {
    pub(crate) fn create(name: &str) -> io::Result<Self> {
        let name = wide(name);
        // SAFETY: plain call. Auto-reset, initially not signalled.
        let raw = unsafe { CreateEventW(null(), 0, 0, name.as_ptr()) };
        // SAFETY: plain call.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let event = Handle::new(raw)?;
        if existed {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an event with this name exists",
            ));
        }
        Ok(Self { event })
    }

    pub(crate) fn open(name: &str) -> io::Result<Self> {
        let name = wide(name);
        // SAFETY: plain call.
        let raw = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                0,
                name.as_ptr(),
            )
        };
        Ok(Self {
            event: Handle::new(raw)?,
        })
    }

    pub(crate) fn signal(&self) {
        // SAFETY: the handle is an open event.
        unsafe { SetEvent(self.event.0) };
    }
}

pub(crate) struct Waiter {
    timer: Option<Handle>,
}

impl Waiter {
    pub(crate) fn precise() -> io::Result<Self> {
        // SAFETY: plain call. A high-resolution timer needs Windows 10 1803;
        // without one, waits count in milliseconds, which is too coarse for
        // a cycle thread, so its absence is an error.
        let raw = unsafe {
            CreateWaitableTimerExW(
                null(),
                null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        Ok(Self {
            timer: Some(Handle::new(raw)?),
        })
    }

    pub(crate) fn coarse() -> Self {
        Self { timer: None }
    }
}

pub(crate) fn wait(
    wake: &Wake,
    word: &AtomicU32,
    seen: u32,
    timeout: Duration,
    waiter: &mut Waiter,
) -> bool {
    // `Instant` reads QueryPerformanceCounter: no allocation, no lock.
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
        match &waiter.timer {
            Some(timer) => {
                // Relative due times are negative, in units of 100 ns.
                let due = -((remaining.as_nanos() / 100).max(1) as i64);
                // SAFETY: the timer handle is open; no completion routine.
                let set = unsafe { SetWaitableTimer(timer.0, &due, 0, None, null_mut(), 0) };
                if set == 0 {
                    // Cannot happen with a valid handle; fall back to
                    // milliseconds rather than spin.
                    wait_ms(wake, remaining);
                    continue;
                }
                let handles = [wake.event.0, timer.0];
                // SAFETY: both handles are open.
                unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            }
            None => wait_ms(wake, remaining),
        }
    }
}

fn wait_ms(wake: &Wake, remaining: Duration) {
    let ms = remaining.as_millis().clamp(1, u128::from(INFINITE - 1)) as u32;
    // SAFETY: the handle is an open event.
    unsafe { WaitForSingleObject(wake.event.0, ms) };
}

pub(crate) struct ProcessWatch {
    process: Handle,
}

impl ProcessWatch {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        // SAFETY: plain call.
        let raw = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        Ok(Self {
            process: Handle::new(raw)?,
        })
    }

    pub(crate) fn has_exited(&self) -> bool {
        // SAFETY: the handle is an open process; a zero timeout never blocks.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 }
    }
}
