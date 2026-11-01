//! The interrupt flag. Its own test binary, since it changes how the whole
//! process answers Ctrl+C.

use std::sync::atomic::Ordering;

#[test]
fn an_interrupt_sets_the_flag_instead_of_ending_the_process() {
    let flag = mare_sys::interrupt_flag().unwrap();
    assert!(!flag.load(Ordering::Acquire));
    // Installing twice hands back the same flag.
    assert!(std::ptr::eq(flag, mare_sys::interrupt_flag().unwrap()));
    // On Windows a console event would reach every process attached to the
    // console, the test runner's included, so only Unix sends one here.
    #[cfg(unix)]
    {
        // SAFETY: plain call; the handler installed above catches it.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        assert!(flag.load(Ordering::Acquire));
    }
}
