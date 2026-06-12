//! Replay half of the flight recorder: re-run a recorded `ux.jsonl.gz`
//! deterministically with no mic and no DSP engine, emitting a fresh trace, so
//! "is the bug fixed?" becomes a diff between two `geom` channels.
//!
//! Three pieces:
//! 1. [`load`] — read a recorded trace back into typed, frame-bucketed records.
//! 2. [`coach::ReplayCoach`] — an `AppCoach` that serves the recorded reads
//!    verbatim (port types, no inverse conversion).
//! 3. [`driver::install`] — per-frame: inject recorded inputs, prime the coach,
//!    drive the clock with recorded deltas, exit after the last frame.

pub mod coach;
pub mod driver;
pub mod load;

pub use coach::{ReplayCoach, SharedReplayCoach};
pub use driver::install;
pub use load::{newest_dir, LoadedTrace};
