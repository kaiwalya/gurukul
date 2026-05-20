//! domain-ports: the trait contracts (ports) that adapters implement.
//!
//! Each domain (Clock, AudioInput, DeviceCatalog, ...) lives in its
//! own module. lib.rs is just the index — open the module file to see
//! the contract.

pub mod clock;

/// Smallest end-to-end use of the ports. Lives here only until PR 3
/// moves it into `apps/cli/main.rs` (it's an app-shaped helper, not a
/// port contract). Kept in this PR purely so the rename stays
/// content-equivalent.
pub fn greet(clock: &dyn clock::Clock) -> String {
    format!("gurukul app-core: t={}ms", clock.now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clock::Clock;

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
