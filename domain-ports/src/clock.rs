//! Clock port: monotonic time source.
//!
//! Every domain that emits timestamps (telemetry, audio block stamps,
//! retry backoffs, ...) takes a `&dyn Clock` rather than calling the
//! OS directly. Adapters supply the real impl; consumer tests can use
//! [`TestClock`] for deterministic time control (behind the
//! `test-util` feature — see crate-root docs).
//!
//! # Contract
//!
//! - **Monotonic:** successive calls to `now_ns()` on the same `Clock`
//!   never return a value lower than a previous call.
//! - **Epoch:** unspecified and adapter-defined. Useful only for
//!   computing deltas, not as a wall-clock or for cross-process
//!   comparison.
//! - **Resolution:** at least milliseconds. Adapters should provide
//!   nanosecond resolution where the OS exposes it.
//! - **Wall-clock independence:** unaffected by NTP, DST, manual
//!   system-clock changes. (`std::time::Instant` provides this on
//!   every supported target.)
//! - **Suspend behavior:** unspecified. Most monotonic clocks pause
//!   during system sleep on macOS; tests should not assume otherwise.
//!
//! # Why nanoseconds?
//!
//! Audio block timestamps and DSP telemetry need sub-millisecond
//! resolution. `now_ms()` is provided as a convenience for log lines
//! and human-readable stamps.

/// Monotonic time source. See module docs for the contract.
///
/// `Send + Sync` because adapters and tests are shared across threads
/// (audio thread reads, UI thread reads, etc.).
pub trait Clock: Send + Sync {
    /// Nanoseconds since the adapter's chosen epoch. Monotonic
    /// non-decreasing.
    fn now_ns(&self) -> u64;

    /// Milliseconds since the adapter's chosen epoch. Default impl
    /// derives from `now_ns()`; adapters generally should not override.
    fn now_ms(&self) -> u64 {
        self.now_ns() / 1_000_000
    }
}

// ---------------------------------------------------------------------
// Test fake — gated. See crate-root docs ("test fakes") for the rule.
//
// `any(test, feature = "test-util")` makes the fake visible to:
//   - this crate's own tests (via `cfg(test)`)
//   - downstream crates that opt in via dev-dependencies with
//     `features = ["test-util"]`
// Production builds (default features, non-test) don't include it.
// ---------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub use fakes::TestClock;

#[cfg(any(test, feature = "test-util"))]
mod fakes {
    use super::Clock;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Deterministic in-memory clock for consumer tests. Honors the
    /// `Clock` contract (monotonic, `Send + Sync`) so tests that
    /// substitute it are exercising the same shape as real adapters.
    ///
    /// Internally an `AtomicU64` so it can be ticked from one thread
    /// while a system-under-test reads it from another.
    pub struct TestClock {
        ns: AtomicU64,
    }

    impl TestClock {
        /// New clock starting at `start_ns`.
        pub fn new(start_ns: u64) -> Self {
            Self {
                ns: AtomicU64::new(start_ns),
            }
        }

        /// Advance the clock by `delta_ns`. Returns the new time.
        pub fn advance_ns(&self, delta_ns: u64) -> u64 {
            self.ns.fetch_add(delta_ns, Ordering::SeqCst) + delta_ns
        }

        /// Advance the clock by `delta_ms`. Returns the new time in ns.
        pub fn advance_ms(&self, delta_ms: u64) -> u64 {
            self.advance_ns(delta_ms * 1_000_000)
        }
    }

    impl Default for TestClock {
        fn default() -> Self {
            Self::new(0)
        }
    }

    impl Clock for TestClock {
        fn now_ns(&self) -> u64 {
            self.ns.load(Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_starts_at_given_epoch() {
        let c = TestClock::new(1_000);
        assert_eq!(c.now_ns(), 1_000);
        assert_eq!(c.now_ms(), 0);
    }

    #[test]
    fn test_clock_advance_is_monotonic() {
        let c = TestClock::default();
        let t0 = c.now_ns();
        c.advance_ns(500);
        let t1 = c.now_ns();
        c.advance_ms(2);
        let t2 = c.now_ns();
        assert!(t0 <= t1);
        assert!(t1 <= t2);
        assert_eq!(t2 - t0, 500 + 2_000_000);
    }

    #[test]
    fn now_ms_rounds_down_from_ns() {
        let c = TestClock::new(1_999_999);
        assert_eq!(c.now_ms(), 1); // 1.999... ms truncates to 1
    }

    /// Sanity-check the contract holds when a generic function takes
    /// `&dyn Clock` — proves trait-object dispatch works and the
    /// default method (`now_ms`) is reachable through the vtable.
    #[test]
    fn dyn_clock_dispatch() {
        fn elapsed_ms(c: &dyn Clock) -> u64 {
            c.now_ms()
        }
        let c = TestClock::new(3_500_000);
        assert_eq!(elapsed_ms(&c), 3);
    }
}
