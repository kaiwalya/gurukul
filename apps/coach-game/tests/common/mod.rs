//! Shared helpers for headless integration tests.
//!
//! `#![allow(dead_code)]`: each integration-test binary compiles this module
//! fresh, and no single binary uses every helper — without the allow, a test
//! that skips one (e.g. `trace_replay_click` defines its own settle loop) trips
//! `dead_code` on the unused `pub fn`.
//!
#![allow(dead_code)]

//! Builds a `Bevy App` with `MinimalPlugins + StatesPlugin` (no
//! renderer, no window), inserts a `FakeCoach` that records every
//! `Command` and serves canned events / feature snapshots, and runs
//! `coach_game::build_app` on top. The fake's call log is shared via
//! an `Arc<Mutex<...>>` so tests can assert on it after `app.update()`.

use bevy::camera::{Camera, ComputedCameraValues, RenderTargetInfo};
use bevy::math::UVec2;
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
    /// Feature history the next `drain_features` call will hand back.
    pub pending_features: Vec<FeatureSnapshot>,
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

    fn drain_features(&self, out: &mut Vec<FeatureSnapshot>) {
        let mut g = self.inner.lock().unwrap();
        out.append(&mut g.pending_features);
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
///
/// `allow(dead_code)` for the per-binary recompile reason noted on
/// `drain_commands`: a layout-only test binary uses `build_layout_test_app`
/// instead and never touches this one.
#[allow(dead_code)]
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

/// Build a **layout-aware** test app: the production wiring on top of the
/// real Bevy UI layout schedule, with no GPU and no window. This is the
/// only harness that populates `ComputedNode` (measured sizes) and
/// `UiGlobalTransform` (global screen positions), so it is the only level
/// that can see the measure→paint seam — the physical/logical frame and
/// clipping. See `ARCHITECTURE.md` ("Testability follows the layers") and
/// `CONTRIBUTING.md` (the layer-3 rules) for *why*.
///
/// Recipe (proven against bevy_ui 0.18): keep `RenderPlugin` so every
/// downstream render plugin's shader/asset init is satisfied, but give
/// wgpu **no backend** so no GPU device is created; drop the window and
/// disable winit. A `Camera2d` carrying a hand-set `RenderTargetInfo` at
/// **scale_factor 2.0** drives layout at a non-unit scale — at 1× a
/// physical/logical frame bug is mathematically invisible and the test
/// would certify the broken code, so 2× is mandatory, not incidental.
///
/// `DefaultPlugins` brings its own `InputPlugin` and `StatesPlugin` is not
/// part of it (state is registered by `build_app`'s `init_state`), so —
/// unlike `build_test_app` — neither is added here.
///
/// `allow(dead_code)` for the same per-binary recompile reason as
/// `drain_commands`: `common/` is rebuilt for every test binary, so a helper
/// only some binaries use warns in the rest.
#[allow(dead_code)]
pub fn build_layout_test_app() -> (App, FakeCoach) {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: bevy::window::ExitCondition::DontExit,
                ..default()
            })
            .set(bevy::render::RenderPlugin {
                render_creation: bevy::render::settings::WgpuSettings {
                    backends: None,
                    ..default()
                }
                .into(),
                ..default()
            })
            .disable::<bevy::winit::WinitPlugin>(),
    );

    // Camera with a hand-set 2× render target — no GPU, no window.
    app.world_mut().spawn((
        Camera2d,
        Camera {
            computed: ComputedCameraValues {
                target_info: Some(RenderTargetInfo {
                    physical_size: UVec2::new(1600, 1200),
                    scale_factor: 2.0,
                }),
                ..default()
            },
            ..default()
        },
    ));

    let fake = FakeCoach::default();
    app.insert_non_send_resource(Coach(Box::new(fake.clone())));

    coach_game::build_app(&mut app);
    (app, fake)
}

/// Drive the layout-aware schedule until the capture→paint loop settles.
/// The chain spans two frames: `capture_pitch_lane_size` runs in
/// `PostUpdate` after `UiSystems::PostLayout` (so it sees this frame's
/// measured lane), and `apply_trace` reads that captured size on the
/// *next* `Update`. A single `app.update()` therefore paints zero trace
/// bodies (no size yet); several updates let the size land, the trace
/// paint, and the layout re-settle on the painted nodes. `allow(dead_code)`
/// for the same per-binary recompile reason as `drain_commands`.
#[allow(dead_code)]
pub fn pump_layout(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

/// Decode a trace file (`traces/<stamp>-ux.jsonl.gz`) and return its contents
/// as a plain `String` of newline-separated JSON lines. Tolerates a missing
/// gzip trailer (killed run) by keeping whatever was decoded before
/// `UnexpectedEof`. Callers pass the file path directly (e.g.
/// `paths::file_path(&root, "run")`).
#[allow(dead_code)]
pub fn decode_trace(trace_path: &std::path::Path) -> String {
    use flate2::read::MultiGzDecoder;
    use std::io::Read;
    let file = std::fs::File::open(trace_path)
        .unwrap_or_else(|e| panic!("could not open {}: {e}", trace_path.display()));
    let mut decoder = MultiGzDecoder::new(file);
    let mut text = String::new();
    match decoder.read_to_string(&mut text) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            // Truncated stream (killed run) — use what was decoded.
        }
        Err(e) => panic!("could not decode {}: {e}", trace_path.display()),
    }
    text
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
