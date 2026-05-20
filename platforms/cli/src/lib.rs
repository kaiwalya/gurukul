//! platform-cli: trait impls for the CLI app.
//!
//! Cross-platform (Linux / macOS / Windows). Nothing OS-specific lives
//! here — when CoreAudio bindings land they go in `platforms/mac`.

use app_core::Clock;
use std::time::Instant;

/// Wall-clock-ish monotonic source for the CLI process. The epoch is
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
