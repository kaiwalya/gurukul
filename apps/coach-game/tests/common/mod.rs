//! Shared helpers for headless integration tests.
//!
//! Builds a `Bevy App` with `MinimalPlugins + StatesPlugin` (no
//! renderer, no window), inserts a `FakeCoach` that records every
//! `Command` and serves canned events / feature snapshots, and runs
//! `coach_game::build_app` on top. The fake's call log is shared via
//! an `Arc<Mutex<...>>` so tests can assert on it after `app.update()`.

use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use coach_game::coach::Coach;
use domain_ports::app_coach::{
    AppCoach, AudioInfo, CoachEvent, Command, FeatureSnapshot, MusicInfo, ShutdownResult,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct FakeCoachState {
    pub commands: Vec<Command>,
    /// Events the next `poll_events` call will hand back, then drain.
    pub pending_events: Vec<CoachEvent>,
    pub latest_features: Option<FeatureSnapshot>,
    pub audio_info: Option<AudioInfo>,
    pub music_info: Option<MusicInfo>,
    pub shutdown_calls: u32,
}

#[derive(Clone, Default)]
pub struct FakeCoach {
    pub inner: Arc<Mutex<FakeCoachState>>,
}

impl AppCoach for FakeCoach {
    fn send_command(&self, cmd: Command) {
        self.inner.lock().unwrap().commands.push(cmd);
    }

    fn poll_events(&self, out: &mut Vec<CoachEvent>) {
        let mut g = self.inner.lock().unwrap();
        out.append(&mut g.pending_events);
    }

    fn shutdown(&self, _timeout: Duration) -> ShutdownResult {
        self.inner.lock().unwrap().shutdown_calls += 1;
        ShutdownResult::Clean
    }

    fn latest_features(&self) -> Option<FeatureSnapshot> {
        self.inner.lock().unwrap().latest_features
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        self.inner.lock().unwrap().audio_info.clone()
    }

    fn music_info(&self) -> Option<MusicInfo> {
        self.inner.lock().unwrap().music_info
    }
}

/// Build a headless test app with the production system wiring + a
/// `FakeCoach` inserted as the `NonSend` resource. Returns the app and
/// a handle to the fake's state for assertions. `InputPlugin` is
/// included so systems that read `Res<ButtonInput<KeyCode>>` (e.g. Esc
/// handlers) resolve their resources.
pub fn build_test_app() -> (App, FakeCoach) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(bevy::input::InputPlugin);

    let fake = FakeCoach::default();
    app.insert_non_send_resource(Coach(Box::new(fake.clone())));

    coach_game::build_app(&mut app);
    (app, fake)
}

/// Pull the most recent commands the fake has seen and clear them.
/// Useful in tests that want to assert per-transition behaviour.
///
/// `allow(dead_code)` because `common/` is recompiled per test binary,
/// so a helper used only by some integration tests warns in the others.
#[allow(dead_code)]
pub fn drain_commands(fake: &FakeCoach) -> Vec<Command> {
    std::mem::take(&mut fake.inner.lock().unwrap().commands)
}

/// Drive the schedule until any pending `NextState` has been applied
/// AND that new state's `OnEnter` schedule has run. In Bevy 0.18 the
/// `StateTransition` schedule runs at the START of the next update
/// after `NextState::set`, so:
///   - Update N: handler writes `NextState::set(InGame)`.
///   - Update N+1: StateTransition consumes it, fires OnExit(MainMenu)
///     + OnEnter(InGame), then Update systems for InGame run.
///
/// One `app.update()` after the click isn't enough to see the new
/// state OR the OnEnter side effects. Two are.
pub fn pump(app: &mut App) {
    app.update();
    app.update();
}
