//! The attach supervisor's signal path, in a binary of its own.
//!
//! It cannot share a process with the rest of the suite: the handler stores
//! into a process-global static, and a test that raises a signal would be seen
//! by every test that asserts nothing has been raised.
//!
//! Nothing here spawns anything. The signals go to this process, which is the
//! only way to prove the registration is real: with the handler missing, the
//! default action for SIGTERM or SIGHUP kills the test binary outright, and the
//! session `attach` supervises would die the same way — before the cleanup that
//! tears down the control master and its live forwards ever runs.

use herdr_portfwd::local_agent::{install_signal_handlers, Interrupts};

#[test]
fn the_signals_that_end_a_session_are_caught_and_handed_to_the_supervisor() {
    let signals = install_signal_handlers();
    assert_eq!(signals.pending(), None, "nothing has been raised yet");

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `raise` targets this process, whose disposition for `signal`
        // was just replaced by a handler that only stores into an atomic.
        let raised = unsafe { libc::raise(signal) };
        assert_eq!(raised, 0, "could not raise signal {signal}");

        assert_eq!(
            signals.pending(),
            Some(signal),
            "signal {signal} never reached the supervisor, so a leaked master \
             with live forwards is what the user would be left with"
        );
    }
}
