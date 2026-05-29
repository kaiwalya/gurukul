//! `join_with_timeout`: the helper [`AppCoach::shutdown`] uses to wait
//! a bounded time for the control-plane thread to exit.
//!
//! On timeout the [`JoinHandle`] is dropped (detaching the thread) and
//! [`ShutdownResult::TimedOut`] is returned. On Unix the OS reaps the
//! detached thread when the process exits, which is fine in practice
//! because heads call `shutdown` near process termination.

use domain_ports::app_coach::ShutdownResult;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Join the control-plane thread with a deadline. On timeout the
/// thread is detached (its `JoinHandle` is dropped), which on Unix
/// leaves it running — but the head will exit shortly anyway, so the
/// OS reaps. Caller has already taken the handle out of `self`.
pub(crate) fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> ShutdownResult {
    if timeout.is_zero() {
        // Zero-timeout shortcut: try a non-blocking is_finished probe
        // a few times so cooperative quick teardowns still report
        // Clean, but don't sit and wait.
        if handle.is_finished() {
            let _ = handle.join();
            return ShutdownResult::Clean;
        }
        return ShutdownResult::TimedOut;
    }

    // Poll: cheap on a teardown path, and avoids dragging in
    // crossbeam_utils just for a join-with-timeout primitive. The
    // control plane sets is_finished as soon as it returns from run().
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if handle.is_finished() {
            let _ = handle.join();
            return ShutdownResult::Clean;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if handle.is_finished() {
        let _ = handle.join();
        ShutdownResult::Clean
    } else {
        // Detach: drop the handle without joining.
        drop(handle);
        ShutdownResult::TimedOut
    }
}
