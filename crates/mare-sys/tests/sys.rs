//! Shared memory, doorbells and process watches, on the system the tests run
//! on.

use std::io;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mare_sys::{Doorbell, HubName, ProcessWatch, SharedMemory, Waiter, current_pid};

/// A hub name no other test, and no other run, uses at the same time.
fn unique_hub() -> HubName {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    HubName::new(&format!("s{:x}-{n}", current_pid() & 0xF_FFFF)).unwrap()
}

#[test]
fn both_mappings_see_the_same_bytes() {
    let name = unique_hub().registry();
    let a = SharedMemory::create(&name, 4096).unwrap();
    let b = SharedMemory::open(&name).unwrap();
    assert!(a.len() >= 4096 && b.len() >= 4096);
    assert_eq!(a.base().as_ptr() as usize % 64, 0);
    // SAFETY: both mappings are 4096 bytes or more and live to the end.
    unsafe {
        assert_eq!(*b.base().as_ptr().add(100), 0, "zero-filled");
        a.base().as_ptr().add(100).write(42);
        assert_eq!(*b.base().as_ptr().add(100), 42);
        b.base().as_ptr().add(4095).write(7);
        assert_eq!(*a.base().as_ptr().add(4095), 7);
    }
}

#[test]
fn only_the_creating_user_may_open_a_segment() {
    let name = unique_hub().registry();
    let created = SharedMemory::create(&name, 64).unwrap();
    assert!(created.is_private_to_user().unwrap());
    // The same user opens it, as a client of the same user does.
    let opened = SharedMemory::open(&name).unwrap();
    assert!(opened.is_private_to_user().unwrap());
}

/// A mapping made without MARÉ, with the default access list of this
/// process's token (which also lets in SYSTEM and administrators), is seen
/// as not private: the check really reads the access list.
#[cfg(windows)]
#[test]
fn a_segment_with_the_default_access_list_is_not_private() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
    let name = unique_hub().registry();
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: plain call with a NUL-terminated name; the handle is closed
    // below.
    let raw = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            64,
            wide.as_ptr(),
        )
    };
    assert!(!raw.is_null());
    let opened = SharedMemory::open(&name).unwrap();
    assert!(!opened.is_private_to_user().unwrap());
    drop(opened);
    // SAFETY: the handle came from CreateFileMappingW above.
    unsafe { CloseHandle(raw) };
}

#[test]
fn a_name_is_created_once_and_must_exist_to_open() {
    let name = unique_hub().registry();
    let first = SharedMemory::create(&name, 64).unwrap();
    let again = SharedMemory::create(&name, 64).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    drop(first);
    let missing = unique_hub().registry();
    assert!(SharedMemory::open(&missing).is_err());
}

#[test]
fn an_unlinked_name_cannot_be_opened_but_the_mapping_stays() {
    let name = unique_hub().registry();
    let memory = SharedMemory::create(&name, 64).unwrap();
    SharedMemory::unlink(&name).unwrap();
    if cfg!(not(windows)) {
        assert!(SharedMemory::open(&name).is_err());
    }
    // SAFETY: the mapping is still there.
    unsafe { memory.base().as_ptr().write(1) };
}

/// A doorbell's counter in its own shared memory, and both ends of it.
fn doorbell_pair() -> (SharedMemory, Doorbell, Doorbell) {
    let hub = unique_hub();
    let memory = SharedMemory::create(&hub.registry(), 64).unwrap();
    let word = memory.base().cast::<AtomicU32>();
    // SAFETY: the memory outlives both doorbells in every test.
    let (owner, other) = unsafe {
        let owner = Doorbell::create(word, &hub.hub_wake()).unwrap();
        let other = Doorbell::open(NonNull::from(word.as_ref()), &hub.hub_wake()).unwrap();
        (owner, other)
    };
    (memory, owner, other)
}

#[test]
fn a_ring_from_another_thread_ends_the_wait() {
    let (_memory, owner, other) = doorbell_pair();
    let other = Arc::new(other);
    let mut waiter = Waiter::precise().unwrap();
    for _ in 0..50 {
        let seen = owner.seen();
        let ringer = Arc::clone(&other);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(200));
            ringer.ring();
        });
        assert!(owner.wait(&mut waiter, seen, Duration::from_secs(5)));
        assert_eq!(owner.seen(), seen.wrapping_add(1));
        thread.join().unwrap();
    }
}

#[test]
fn a_ring_before_the_wait_is_not_lost() {
    let (_memory, owner, other) = doorbell_pair();
    let mut waiter = Waiter::coarse();
    let seen = owner.seen();
    other.ring();
    let start = Instant::now();
    assert!(owner.wait(&mut waiter, seen, Duration::from_secs(5)));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_precise_wait_times_out_on_time() {
    let (_memory, owner, _other) = doorbell_pair();
    let mut waiter = Waiter::precise().unwrap();
    let mut took = Vec::new();
    for _ in 0..21 {
        let seen = owner.seen();
        let start = Instant::now();
        assert!(!owner.wait(&mut waiter, seen, Duration::from_micros(500)));
        took.push(start.elapsed());
    }
    took.sort();
    assert!(took[0] >= Duration::from_micros(500));
    // Generous bounds, for loaded CI machines; on an idle machine the median
    // is a few tens of microseconds over. A wait counted in whole
    // milliseconds (or in 15.6 ms scheduler ticks) would fail the median.
    let median = took[took.len() / 2];
    assert!(median < Duration::from_millis(3), "median wait {median:?}");
    assert!(took[took.len() - 1] < Duration::from_millis(50), "{took:?}");
}

#[test]
fn a_process_is_seen_to_end() {
    let me = ProcessWatch::open(current_pid()).unwrap();
    assert!(!me.has_exited());

    // The test binary itself, asked only to list its tests, exits at once.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let watch = ProcessWatch::open(child.id()).unwrap();
    child.wait().unwrap();
    assert!(watch.has_exited());
    assert_eq!(watch.is_exact(), cfg!(any(windows, target_os = "linux")));
}

/// A process that has ended but that its parent has not reaped yet (a
/// zombie) has ended, for a watch that sees exactly.
#[test]
fn an_unreaped_process_is_seen_to_end_where_the_watch_is_exact() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let watch = ProcessWatch::open(child.id()).unwrap();
    if !watch.is_exact() {
        eprintln!("skipped: this system's watch checks the id");
        child.wait().unwrap();
        return;
    }
    // Not reaped: wait for the end without calling `wait`.
    let start = Instant::now();
    while !watch.has_exited() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "never seen to end"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    child.wait().unwrap();
}

#[test]
fn a_thread_is_promoted_and_let_go() {
    std::thread::spawn(|| {
        for round in 0..2 {
            // A system without the scheduling service (some Windows Server
            // installations) refuses; that is reported, not hidden.
            match mare_sys::promote_current_thread() {
                Ok(promoted) => {
                    eprintln!("promoted: {}", promoted.is_promoted());
                    // Windows and macOS let any user ask; Linux depends on
                    // the user's real-time limit, so it is only reported.
                    if cfg!(any(windows, target_os = "macos")) {
                        assert!(promoted.is_promoted());
                    }
                }
                Err(error) => {
                    eprintln!("skipped: cannot promote a thread here: {error}");
                    return;
                }
            }
            // The second round promotes again once the first is let go.
            let _ = round;
        }
    })
    .join()
    .unwrap();
}
