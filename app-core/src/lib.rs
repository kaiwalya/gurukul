//! app-core: platform-agnostic application logic.
//!
//! Defines the traits that platforms implement (`Clock`, and later
//! `AudioInput`, `AudioOutput`, `DeviceCatalog`, ...) and the routines
//! that glue them to the DSP engine. The app entry point on each
//! platform constructs the platform impls and hands them to functions
//! defined here.
//!
//! This crate has no OS dependencies and no engine dependency yet —
//! both are added as the layering proves itself.

/// Monotonic time source. Platforms supply an impl; tests use a fake.
pub trait Clock {
    /// Milliseconds since some platform-defined epoch. Must be
    /// monotonic — successive calls never decrease.
    fn now_ms(&self) -> u64;
}

/// Smallest end-to-end use of the trait: produce a one-line greeting
/// stamped with the platform clock. Proves the wiring shape; replaced
/// by real app-core entry points as the layers grow.
pub fn greet(clock: &dyn Clock) -> String {
    format!("gurukul app-core: t={}ms", clock.now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClock(u64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn greet_uses_clock() {
        let c = FakeClock(42);
        assert_eq!(greet(&c), "gurukul app-core: t=42ms");
    }
}
