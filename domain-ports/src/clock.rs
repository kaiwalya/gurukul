//! Clock port: monotonic time source.
//!
//! PR 3 gives this teeth (now_ns, monotonicity contract, TestClock).
//! For now this is the minimum content moved verbatim from the old
//! lib.rs so PR 2 stays a pure rename.

/// Monotonic time source. Platforms supply an impl; tests use a fake.
pub trait Clock {
    /// Milliseconds since some platform-defined epoch. Must be
    /// monotonic — successive calls never decrease.
    fn now_ms(&self) -> u64;
}
