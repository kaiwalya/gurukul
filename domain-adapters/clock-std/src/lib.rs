//! adapter-clock-std: the Clock port adapter backed by `std::time`.
//!
//! `std::time::Instant` wraps the monotonic OS clock on every supported
//! target (macOS `mach_absolute_time`, Linux `CLOCK_MONOTONIC`, Windows
//! `QueryPerformanceCounter`), so a single Rust impl covers them all.
//!
//! Apps call [`new`] to get an `impl Clock` — the concrete type is
//! intentionally private so adapter changes (e.g. ever needing
//! `mach_continuous_time` on macOS to count during sleep) don't ripple
//! into callers. If that day comes, add `#[cfg(target_os = "macos")]`
//! arms inside [`new`]; the public surface stays the same.

use domain_ports::clock::Clock;
use std::time::Instant;

/// Build the platform's monotonic clock. Cheap (a single `Instant::now()`).
///
/// The returned clock's epoch is the moment of this call — useful only
/// for deltas, as the trait contract specifies. Call once at app
/// startup and pass `&dyn Clock` everywhere.
pub fn new() -> impl Clock {
    StdClock {
        start: Instant::now(),
    }
}

struct StdClock {
    start: Instant,
}

impl Clock for StdClock {
    fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn fresh_clock_is_near_zero() {
        let c = new();
        // Construction + one method call should fit well under 10ms
        // even under heavy CI load.
        assert!(c.now_ns() < 10_000_000, "got {} ns", c.now_ns());
    }

    #[test]
    fn now_ns_is_monotonic_across_calls() {
        let c = new();
        let mut last = c.now_ns();
        for _ in 0..100 {
            let cur = c.now_ns();
            assert!(cur >= last, "regressed: {cur} < {last}");
            last = cur;
        }
    }

    #[test]
    fn sleep_advances_clock_by_at_least_the_sleep_duration() {
        let c = new();
        let t0 = c.now_ns();
        sleep(Duration::from_millis(10));
        let t1 = c.now_ns();
        let delta_ms = (t1 - t0) / 1_000_000;
        assert!(delta_ms >= 10, "delta was {delta_ms}ms, expected >= 10");
        // Generous upper bound to catch wall-clock-vs-monotonic bugs
        // without making the test flaky under load.
        assert!(
            delta_ms < 1_000,
            "delta was {delta_ms}ms, suspiciously large"
        );
    }
}
