//! Phase-1 recorder coverage (layout-aware, scale factor 2.0).
//!
//! The only level that can prove the `geom` channel records *computed* layout —
//! physical sizes and global positions — because it runs the real Bevy UI
//! schedule. It runs at **2×** for the same reason `time_graph_layout.rs` does:
//! at 1× physical and logical pixels coincide, so the frame the `geom` record
//! exists to expose (physical size + scale factor recorded together, so a 2×
//! confusion is visible *as data*) would be invisible.
//!
//! The headline assertion: the pitch-lane `geom` record carries a physical
//! `size_px` and `scale_factor == 2.0`, and the implied logical size
//! (`size_px / scale_factor`) matches the lane's real laid-out logical size.
//! That is the exact pair a reader uses to catch a frame bug, asserted end to
//! end through the recorder.

mod common;

use std::fs;
use std::path::PathBuf;

use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};
use coach_game::menu::main_menu::NewGameButton;
use coach_game::trace::{self, TracePlugin};
use coach_game::widgets::time_graph::TimeGraphPitchLane;
use common::{build_layout_test_app, pump, pump_layout, FakeCoach};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::pitch::{PitchLog2, PitchLog2Interval};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};
use serde_json::Value;

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("gurukul-trace-layout-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn snapshot(hop_index: u64, t_ms: u64, pitch: PitchLog2) -> FeatureSnapshot {
    FeatureSnapshot {
        hop_index,
        f0_hz: pitch.to_hz(),
        confidence: 0.8,
        onset: 0.0,
        breath: 0.0,
        vibrato_rate: 0.0,
        vibrato_depth: 0.0,
        t_ms,
    }
}

fn music(octave: i32) -> MusicInfo {
    let tuning = TuningAbsolute::new(TuningKind::TwelveTet.intervals(), PitchLog2Interval(0.17));
    MusicInfo {
        scale: Scale::new(
            ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
            tuning.shift_up(3),
            octave,
        ),
    }
}

fn publish_music(fake: &FakeCoach, info: MusicInfo) {
    let mut state = fake.inner.lock().unwrap();
    state.music_info = Some(info);
    state
        .pending_events
        .push(CoachEvent::SessionConfigured { scale: info.scale });
}

fn read_records(root: &std::path::Path) -> Vec<Value> {
    let path = trace::file_path(root, "run");
    let text = common::decode_trace(&path);
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is valid json"))
        .collect()
}

#[test]
fn geom_record_carries_physical_size_and_scale_factor_at_2x() {
    let root = temp_root();
    let (mut app, fake) = build_layout_test_app();
    coach_game::trace::install_recording_coach_over_existing(app.world_mut());
    app.add_plugins(TracePlugin {
        root: root.clone(),
        stamp: "run".to_string(),
        wall_start: "2026-06-10 00:00:00 UTC".to_string(),
        replay_of: None,
    });

    pump(&mut app);
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);

    publish_music(&fake, music(4));
    fake.inner.lock().unwrap().pending_features = vec![
        snapshot(0, 1_000, PitchLog2(8.0)),
        snapshot(1, 1_010, PitchLog2(8.2)),
    ];
    pump_layout(&mut app);

    // The lane's real laid-out logical size, straight off the live ECS, to
    // compare the recorded physical size against.
    let world = app.world_mut();
    let (xform, node) = world
        .query_filtered::<(&UiGlobalTransform, &ComputedNode), With<TimeGraphPitchLane>>()
        .single(world)
        .expect("pitch lane laid out");
    let _ = xform;
    let physical = node.size;
    let inv = node.inverse_scale_factor;
    assert!(
        (inv - 0.5).abs() < 1e-4,
        "harness must run at 2x (inverse_scale_factor 0.5), got {inv}"
    );
    let logical = physical * inv;

    let records = read_records(&root);
    let lane = records
        .iter()
        .filter(|r| r["k"] == "geom")
        .find(|r| {
            r["path"]
                .as_str()
                .map(|p| p.ends_with("time_graph/pitch_lane"))
                .unwrap_or(false)
        })
        .expect("a geom record for the pitch lane keyed by widget path");

    // Scale factor recorded.
    let sf = lane["scale_factor"].as_f64().expect("scale_factor field") as f32;
    assert!(
        (sf - 2.0).abs() < 1e-4,
        "scale_factor should be 2.0, got {sf}"
    );

    // Physical size recorded matches the live physical size.
    let size_px = lane["size_px"].as_array().expect("size_px array");
    let rec_w = size_px[0].as_f64().unwrap() as f32;
    let rec_h = size_px[1].as_f64().unwrap() as f32;
    assert!(
        (rec_w - physical.x).abs() < 1.0 && (rec_h - physical.y).abs() < 1.0,
        "recorded physical size {rec_w}x{rec_h} should match live {physical:?}"
    );

    // The whole point: the reader derives logical from (physical, scale) and a
    // 2x frame bug would show up here. Physical is exactly 2x logical.
    assert!(
        (rec_w / sf - logical.x).abs() < 1.0 && (rec_h / sf - logical.y).abs() < 1.0,
        "recorded physical/scale should recover the lane's logical size {logical:?}"
    );

    let _ = fs::remove_dir_all(&root);
}
