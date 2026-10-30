//! Windows: file mappings backed by the paging file, auto-reset events, and
//! process handles.

use std::io;
use std::ptr::{NonNull, null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};
use windows_sys::Win32::System::Threading::{
    AVRT_PRIORITY_HIGH, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    AvSetMmThreadPriority, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW,
    CreateWaitableTimerExW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW,
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS,
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

/// The current user's security identifier, as the process token gives it.
struct UserSid {
    // TOKEN_USER and the SID after it; u64 keeps the pointer in it aligned.
    buffer: Vec<u64>,
}

impl UserSid {
    fn current() -> io::Result<Self> {
        let mut token = null_mut();
        // SAFETY: plain call; `token` receives a handle closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle::new(token)?;
        let mut len = 0u32;
        // SAFETY: asks only for the size, which the call writes to `len`.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len) };
        let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buffer` holds `len` bytes, aligned for TOKEN_USER.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { buffer })
    }

    fn sid(&self) -> PSID {
        // SAFETY: the buffer holds a TOKEN_USER, filled by `current`.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn to_string(&self) -> io::Result<String> {
        let mut text = null_mut();
        // SAFETY: the SID is valid; `text` receives a string freed below.
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a NUL-terminated string from the call above.
        let len = unsafe { (0..).take_while(|&i| *text.add(i) != 0).count() };
        // SAFETY: as above, `len` units long.
        let string = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc.
        unsafe { LocalFree(text.cast()) };
        Ok(string)
    }
}

/// A security descriptor whose access list grants the current user, and
/// nobody else, full access: what every object MARÉ creates is given, so
/// that another account on the machine (or a service) cannot open it.
struct OwnerOnly {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl OwnerOnly {
    fn new() -> io::Result<Self> {
        // A protected access list (P: nothing inherited) with one entry:
        // allow (A) generic all (GA) to the user's SID.
        let sddl = wide(&format!(
            "D:P(A;;GA;;;{})",
            UserSid::current()?.to_string()?
        ));
        let mut descriptor = null_mut();
        // SAFETY: plain call; `descriptor` receives memory freed on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion call with LocalAlloc.
        unsafe { LocalFree(self.descriptor) };
    }
}

/// Whether the access list of the kernel object `handle` has exactly one
/// entry, which allows the current user.
fn private_to_user(handle: HANDLE) -> io::Result<bool> {
    let user = UserSid::current()?;
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: plain call; `descriptor` receives memory freed below, and
    // `dacl` points into it.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: `dacl` is valid while `descriptor` is; an ACE of type 0 is an
    // ACCESS_ALLOWED_ACE, whose SID starts at `SidStart`.
    let private = unsafe {
        let mut ace = null_mut();
        !dacl.is_null()
            && (*dacl).AceCount == 1
            && GetAce(dacl, 0, &mut ace) != 0
            && (*ace.cast::<ACCESS_ALLOWED_ACE>()).Header.AceType == 0
            && EqualSid(
                std::ptr::addr_of_mut!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast(),
                user.sid(),
            ) != 0
    };
    // SAFETY: allocated by GetSecurityInfo with LocalAlloc.
    unsafe { LocalFree(descriptor) };
    Ok(private)
}

pub(crate) struct SharedMemory {
    mapping: Handle,
    base: NonNull<u8>,
    len: usize,
}

impl SharedMemory {
    pub(crate) fn create(name: &str, size: usize) -> io::Result<Self> {
        let name = wide(name);
        let size = size as u64;
        let owner = OwnerOnly::new()?;
        let attributes = owner.attributes();
        // SAFETY: plain call; the name and the attributes outlive it.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &attributes,
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
            mapping,
            base,
            len: info.RegionSize,
        })
    }

    pub(crate) fn is_private_to_user(&self) -> io::Result<bool> {
        private_to_user(self.mapping.0)
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
        let owner = OwnerOnly::new()?;
        let attributes = owner.attributes();
        // SAFETY: as above; the attributes outlive the call.
        let raw = unsafe { CreateEventW(&attributes, 0, 0, name.as_ptr()) };
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

    pub(crate) fn signal(&self, _word: &AtomicU32) {
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

    pub(crate) fn exact(&self) -> bool {
        true
    }
}

pub(crate) struct RealtimeThread {
    task: HANDLE,
}

impl RealtimeThread {
    pub(crate) fn promote() -> io::Result<Self> {
        let name = wide("Pro Audio");
        let mut index = 0u32;
        // SAFETY: plain call; the name is NUL-terminated and outlives it.
        let task = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index) };
        if task.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `task` was just returned for this thread. A failure here
        // leaves the task's own priority, which is already raised.
        unsafe { AvSetMmThreadPriority(task, AVRT_PRIORITY_HIGH) };
        Ok(Self { task })
    }

    pub(crate) fn is_promoted(&self) -> bool {
        true
    }
}

impl Drop for RealtimeThread {
    fn drop(&mut self) {
        // SAFETY: the handle came from `AvSetMmThreadCharacteristicsW` on
        // this thread, which the guard cannot leave.
        unsafe { AvRevertMmThreadCharacteristics(self.task) };
    }
}
