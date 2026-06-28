//! Headless ECS tests for the `--replay-audio` WAV-end auto-return-to-menu.
//!
//! These tests use the production `build_app` schedule (level-2 harness) and
//! manually insert `ReplayAudioEnd` + its systems, mirroring what `run_live`
//! does when `--replay-audio` is supplied. The `FakeCoach` drives
//! `FeatureDrainCount` by seeding `pending_features`.

mod common;

use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy::time::TimeUpdateStrategy;
use coach_game::coach::Coach;
use coach_game::replay_audio::{
    detect_wav_end, reset_detector, ReplayAudioEnd, WAV_END_THRESHOLD_SECS,
};
use coach_game::state::AppState;
use common::{pump, FakeCoach};
use domain_ports::app_coach::FeatureSnapshot;

fn current_state(app: &App) -> AppState {
    *app.world().resource::<State<AppState>>().get()
}

/// Build a test app with the replay-audio detector wired in, starting in
/// `InGame`. Returns the app and the fake coach for feature seeding.
fn build_replay_test_app() -> (App, FakeCoach) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(bevy::input::InputPlugin);

    let fake = FakeCoach::default();
    app.insert_non_send_resource(Coach(Box::new(fake.clone())));

    coach_game::build_app(&mut app);

    // Wire the replay-audio detector (mirrors run_live when replay_audio.is_some()).
    app.insert_resource(ReplayAudioEnd::default())
        .add_systems(OnEnter(AppState::InGame), reset_detector)
        .add_systems(
            Update,
            detect_wav_end
                .after(coach_game::coach::drain_events)
                .run_if(in_state(AppState::InGame)),
        );

    // Start in InGame so the detector is live from the first update.
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::InGame);
    // Two updates: one to apply the NextState transition, one to fire OnEnter.
    app.update();
    app.update();

    (app, fake)
}

/// Push a single dummy `FeatureSnapshot` into the fake so the next
/// `drain_events` call sees a non-zero drain count.
fn push_hop(fake: &FakeCoach) {
    fake.inner
        .lock()
        .unwrap()
        .pending_features
        .push(FeatureSnapshot {
            hop_index: 0,
            f0_hz: 440.0,
            confidence: 1.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_amplitude: 0.0,
            vibrato_phase: 0.0,
            vibrato_t_ms: 0,
            t_ms: 0,
        });
}

/// Advance the app by `n` frames where each frame has `delta_secs` of
/// simulated wall time, without feeding any features (zero-drain).
///
/// Uses `TimeUpdateStrategy::ManualDuration` so Bevy's time-update system
/// feeds the specified delta into `Time<()>` (= `Res<Time>`), which is what
/// `detect_wav_end` accumulates into `silent_secs`.
fn pump_silent(app: &mut App, n: usize, delta_secs: f32) {
    let duration = std::time::Duration::from_secs_f32(delta_secs);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(duration));
    for _ in 0..n {
        app.update();
    }
    app.insert_resource(TimeUpdateStrategy::Automatic);
}

// ── Test 1: draining hops keeps state InGame ──────────────────────────────

#[test]
fn draining_hops_keeps_in_game() {
    let (mut app, fake) = build_replay_test_app();
    assert_eq!(current_state(&app), AppState::InGame);

    // Feed several frames with hops — seen_audio becomes true, silent_secs
    // stays at 0.  State must remain InGame throughout.
    for _ in 0..10 {
        push_hop(&fake);
        pump(&mut app);
        assert_eq!(
            current_state(&app),
            AppState::InGame,
            "state should stay InGame while hops are draining"
        );
    }
}

// ── Test 2: zero-drain past threshold transitions to MainMenu ─────────────

#[test]
fn silence_past_threshold_returns_to_main_menu() {
    let (mut app, fake) = build_replay_test_app();

    // Establish seen_audio = true with one hop.
    push_hop(&fake);
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::InGame);

    // Now go silent for just over the threshold.  Use 60 frames at 1/30s each
    // = 2 s > WAV_END_THRESHOLD_SECS (1 s).
    let frame_dt = 1.0 / 30.0_f32;
    let frames_needed = ((WAV_END_THRESHOLD_SECS / frame_dt) as usize) + 5;
    pump_silent(&mut app, frames_needed, frame_dt);

    assert_eq!(
        current_state(&app),
        AppState::MainMenu,
        "state should return to MainMenu after sustained silence"
    );
}

// ── Test 3: pre-roll silence (before any audio) stays InGame ─────────────

#[test]
fn pre_roll_silence_stays_in_game() {
    let (mut app, _fake) = build_replay_test_app();

    // No hops at all — seen_audio is never set.  Run well past the threshold.
    let frame_dt = 1.0 / 30.0_f32;
    let frames = ((WAV_END_THRESHOLD_SECS * 3.0 / frame_dt) as usize) + 5;
    pump_silent(&mut app, frames, frame_dt);

    assert_eq!(
        current_state(&app),
        AppState::InGame,
        "pre-roll silence (no audio yet) must NOT trigger auto-return"
    );
}
