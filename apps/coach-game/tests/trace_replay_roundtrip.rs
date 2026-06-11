//! Phase-2 determinism contract: record → replay → the `geom` channels match.
//!
//! This is the executable form of "a fix is verified by diffing two traces".
//! It records a synthetic run in the layout harness at 2× (the only level that
//! produces `ComputedNode` geometry), replays that trace through the
//! `ReplayCoach` and driver in a *second* layout app, and asserts the two runs'
//! `geom` records are equal modulo the `run` header.
//!
//! The harness fixes the scale factor to 2.0 via the camera's `RenderTargetInfo`
//! (there is no window), so replay's live-only window-override step is correctly
//! not exercised here — the controlled environment the plan's "Known
//! nondeterminism" note relies on. What *is* exercised end to end: the schema-3
//! coach channel, verbatim feature replay, input injection, the manual clock,
//! and the geom recorder running identically across both passes.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bevy::camera::{Camera, ComputedCameraValues, RenderTargetInfo};
use bevy::input::keyboard::KeyboardInput;
use bevy::input::ButtonState;
use bevy::math::UVec2;
use bevy::prelude::*;
use bevy::window::WindowEvent;
use coach_game::trace::replay;
use coach_game::trace::TracePlugin;
use common::{build_layout_test_app, pump, pump_layout};
use domain_ports::app_coach::FeatureSnapshot;
use serde_json::Value;

fn temp_root(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gurukul-roundtrip-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn snapshot(hop_index: u64, t_ms: u64, f0_hz: f32) -> FeatureSnapshot {
    FeatureSnapshot {
        hop_index,
        f0_hz,
        confidence: 0.7,
        onset: 0.0,
        breath: 0.0,
        vibrato_rate: 0.0,
        vibrato_depth: 0.0,
        t_ms,
    }
}

/// A geom record reduced to its identity (path) + the geometry fields the
/// determinism contract compares. Frame index is deliberately excluded: replay
/// runs one extra (exit) frame, so frame numbers shift by a constant, but the
/// *set* of (path → geometry) a run paints must be identical.
type GeomKey = (String, String, String, String, bool);

fn geom_set(root: &Path) -> BTreeMap<GeomKey, ()> {
    let text = fs::read_to_string(root.join("run").join("ux.jsonl"))
        .expect("trace file should exist after a run");
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let r: Value = serde_json::from_str(line).expect("valid json");
        if r["k"] != "geom" {
            continue;
        }
        let key = (
            r["path"].as_str().unwrap_or("").to_string(),
            r["size_px"].to_string(),
            r["rect_px"].to_string(),
            r["scale_factor"].to_string(),
            r["gone"].as_bool().unwrap_or(false),
        );
        out.insert(key, ());
    }
    out
}

/// Record pass: a recording-wrapped `FakeCoach` in the 2× layout harness, fed
/// canned features + an injected key, pumped into `root`.
///
/// The run stays on the **main menu** — it does not click into the game. A
/// state transition here is driven by spawning an `Interaction::Pressed`
/// component, which is *not* an input message, so replay (which only re-emits
/// recorded inputs) couldn't reproduce it — the menu would despawn in the
/// recording but not the replay, and the divergence would be an artifact of the
/// test's out-of-band transition, not of the replay machinery. Holding one
/// state keeps the comparison honest: every painted widget, and the absence of
/// any spurious `gone`, must match.
fn record(root: &Path) {
    let (mut app, fake) = build_layout_test_app();
    coach_game::trace::install_recording_coach_over_existing(app.world_mut());
    app.add_plugins(TracePlugin {
        root: root.to_path_buf(),
        run_dir: "run".to_string(),
        wall_start: "2026-06-10 00:00:00 UTC".to_string(),
        replay_of: None,
    });

    pump(&mut app);

    // Canned features flow through the coach channel (and so must replay
    // verbatim); an injected F8 gives the `input` channel something the driver
    // must re-emit (F8 is in the driver's decode table). Schema 3: the recorder
    // taps the canonical `WindowEvent` stream, so inject the combined event.
    fake.inner.lock().unwrap().pending_features =
        vec![snapshot(0, 1_000, 220.0), snapshot(1, 1_010, 222.0)];
    app.world_mut()
        .write_message(WindowEvent::KeyboardInput(KeyboardInput {
            key_code: KeyCode::F8,
            logical_key: bevy::input::keyboard::Key::Character("8".into()),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        }));
    pump_layout(&mut app);
    app.update();
}

/// Replay pass: a 2× layout app with **no** `FakeCoach` — `replay::install`
/// inserts the `ReplayCoach` + driver — plus the recorder writing into `root`.
/// Pumped enough frames to serve the whole loaded trace.
fn replay(src: &Path, root: &Path) {
    let trace = replay::load::load(src).expect("load the recorded trace");
    let frames = trace.frames.len();

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

    // Same hand-set 2× render target as `build_layout_test_app`, so geometry is
    // produced at the same scale the recording used.
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

    // Inserts the ReplayCoach as the `Coach` handle, the driver, and the manual
    // clock — no FakeCoach in this app.
    replay::install(&mut app, trace, /* hold */ true);

    coach_game::build_app(&mut app);

    app.add_plugins(TracePlugin {
        root: root.to_path_buf(),
        run_dir: "run".to_string(),
        wall_start: "2026-06-10 00:00:01 UTC".to_string(),
        replay_of: Some("src".to_string()),
    });

    // Drive at least as many updates as the trace has frames, plus the
    // capture→paint settling the layout loop needs (`pump_layout`'s 6).
    for _ in 0..(frames + 8) {
        app.update();
    }
}

#[test]
fn geom_channel_survives_record_then_replay() {
    let dir_a = temp_root("record");
    let dir_b = temp_root("replay");

    record(&dir_a);
    let recorded = geom_set(&dir_a);
    assert!(
        !recorded.is_empty(),
        "the recording pass should paint some geometry"
    );

    // The recorder writes into `<dir_a>/run/`; the loader reads a run dir
    // directly.
    replay(&dir_a.join("run"), &dir_b);
    let replayed = geom_set(&dir_b);

    // The contract: every (path, size, rect, scale, gone) the recording painted
    // is painted identically on replay, and replay invents nothing new.
    let only_recorded: Vec<_> = recorded
        .keys()
        .filter(|k| !replayed.contains_key(*k))
        .collect();
    let only_replayed: Vec<_> = replayed
        .keys()
        .filter(|k| !recorded.contains_key(*k))
        .collect();
    assert!(
        only_recorded.is_empty() && only_replayed.is_empty(),
        "geom channels diverged.\n  only in recording: {only_recorded:#?}\n  only in replay: {only_replayed:#?}"
    );

    let _ = fs::remove_dir_all(&dir_a);
    let _ = fs::remove_dir_all(&dir_b);
}
