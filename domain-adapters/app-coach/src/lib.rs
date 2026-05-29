//! adapter-app-coach: the canonical [`AppCoach`] implementation.
//!
//! v1 implements just the control plane plus a direct AudioCapture
//! stream — no pitch detection, no separate data-plane worker. See
//! `docs/SPEC-AppCoach.md` §0 and §7 for the v1 scope and §13 for the
//! Phase 2 work that builds on this.
//!
//! Hosts wire peripheral adapters (clock, telemetry, audio devices,
//! audio capture) into [`AppCoachDeps`] and call [`new`] to get an
//! `impl AppCoach`. They then drive the coach via [`AppCoach::send_command`]
//! and drain state changes via [`AppCoach::poll_events`].

use domain_ports::app_coach::{AppCoach, AppCoachDeps, CoachEvent, Command, ShutdownResult};
use std::time::Duration;

/// Build the canonical [`AppCoach`].
///
/// **TODO(PR 18):** this returns a stub that panics on every call.
/// PR 18 fills in the control-plane thread, drain loop, and session
/// state machine. The port + the new boundary land in this PR (PR 17)
/// so the trait shape can be reviewed independently of the
/// implementation.
pub fn new(_deps: AppCoachDeps) -> impl AppCoach {
    StubCoach
}

struct StubCoach;

impl AppCoach for StubCoach {
    fn send_command(&self, _cmd: Command) {
        todo!("adapter-app-coach v2 lands in PR 18")
    }

    fn poll_events(&self, _out: &mut Vec<CoachEvent>) {
        todo!("adapter-app-coach v2 lands in PR 18")
    }

    fn shutdown(&self, _timeout: Duration) -> ShutdownResult {
        todo!("adapter-app-coach v2 lands in PR 18")
    }
}
