//! AppCoach port: the singing-coach product's headless behaviour.
//!
//! The coach is the *product*, not a host. It owns the lifecycle of a
//! coaching session (boot, run, shutdown) and emits telemetry along
//! the way. A host (`coach-cli`, future `coach-mac`, `coach-watch`)
//! wires the peripheral adapters (clock, telemetry, ...) and calls
//! [`AppCoach::main`] from its platform entry point.
//!
//! # Why a port for the app?
//!
//! Hosts are platform shells — `fn main()`, `@main struct App`, watch
//! glance, etc. Without this port the coach's behaviour would scatter
//! across each host's entry point and drift. With it, each host stays
//! a wire-only adapter and the coach behaviour lives in
//! `adapter-app-coach`, exercised by tests that inject fakes through
//! [`AppCoachDeps`].
//!
//! # Shape
//!
//! [`AppCoachDeps`] is a plain-old-data bag of `Arc<dyn Port>`s the
//! coach needs. Hosts construct it once at boot; the coach uses it for
//! the duration of `main`. Add fields here as new ports land.

use crate::clock::Clock;
use crate::telemetry::Telemetry;
use std::sync::Arc;

/// Everything the coach needs to run, supplied by its host.
///
/// Host code (`apps/<host>/src/main.rs`) builds this once after wiring
/// peripheral adapters, then calls [`AppCoach::main`].
pub struct AppCoachDeps {
    pub clock: Arc<dyn Clock>,
    pub telemetry: Arc<dyn Telemetry>,
    /// `CARGO_PKG_VERSION` of the *host* binary. Lets the coach stamp
    /// boot events with the version the user is actually running,
    /// rather than the adapter's own version (which is meaningless to
    /// the warehouse).
    pub host_version: &'static str,
}

/// The coach's headless entry point.
///
/// Blocking: returns when the coaching session ends. The host is
/// responsible for whatever happens after (exit, event loop, ...).
pub trait AppCoach: Send + Sync {
    fn main(&self, deps: AppCoachDeps);
}
