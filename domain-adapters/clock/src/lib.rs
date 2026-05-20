//! domain-adapter-clock: the Clock port adapter for native targets.
//!
//! `std::time::Instant` wraps the monotonic OS clock on every supported
//! target (macOS `mach_absolute_time`, Linux `CLOCK_MONOTONIC`, Windows
//! `QueryPerformanceCounter`), so a single Rust impl covers them all.
//! PR 3 wraps construction in a `pub fn new() -> impl Clock` factory
//! and tightens the contract; this PR is the rename only.

use domain_ports::clock::Clock;
use std::time::Instant;

/// Wall-clock-ish monotonic source for the host process. The epoch is
/// process start, which is fine for the trait contract (monotonic,
/// platform-defined epoch).
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}
