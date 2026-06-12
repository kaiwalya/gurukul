//! Phase-1 recorder coverage (headless).
//!
//! Builds the production wiring on `MinimalPlugins` with a `FakeCoach` wrapped
//! in the recording decorator and a `TracePlugin` pointed at a temp dir, drives
//! a few frames with canned features + an injected keypress, then reads the
//! emitted `ux.jsonl` back and asserts the `coach` and `input` channels carry
//! what the app saw. This is the level-2 view of the recorder: it proves the
//! port-side decorator + the buffer-drain system + the writer round-trip data
//! to disk. It is *blind to* geometry — `MinimalPlugins` runs no layout, so
//! `geom` is asserted at layer 3 in `trace_recorder_layout.rs`.

mod common;

use std::fs;
use std::path::PathBuf;

use bevy::input::keyboard::KeyboardInput;
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy::window::WindowEvent;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::trace::{self, TracePlugin};
use common::{pump, FakeCoach};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot};
use serde_json::Value;

/// A unique temp directory for one test run. No `tempfile` dep in the tree, so
/// we mint a name under the OS temp dir from the test's own address-space — a
/// counter keeps two tests in the same binary from colliding.
fn temp_root(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gurukul-trace-test-{tag}-{}-{n}",
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

/// Build a headless app whose `Coach` is a recording-wrapped `FakeCoach`, with
/// the trace plugin writing to `root`. Mirrors `common::build_test_app` but
/// installs the decorator + plugin (which production does in `main.rs`).
fn build_recording_app(root: &std::path::Path) -> (App, FakeCoach) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(bevy::input::InputPlugin);

    let fake = FakeCoach::default();
    coach_game::trace::install_recording_coach(app.world_mut(), Box::new(fake.clone()));

    coach_game::build_app(&mut app);

    app.add_plugins(TracePlugin {
        root: root.to_path_buf(),
        stamp: "run".to_string(),
        wall_start: "2026-06-10 00:00:00 UTC".to_string(),
        replay_of: None,
    });
    (app, fake)
}

/// Read every record line of the trace as JSON values.
fn read_records(root: &std::path::Path) -> Vec<Value> {
    let path = trace::file_path(root, "run");
    let text = common::decode_trace(&path);
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is valid json"))
        .collect()
}

#[test]
fn records_run_header_frame_coach_and_input_channels() {
    let root = temp_root("headless");
    let (mut app, fake) = build_recording_app(&root);

    // Frame 0..: enter the game so the feature drain runs in InGame.
    pump(&mut app);
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);

    // Hand the coach a configured scale + a couple of feature snapshots and an
    // event, then inject an F-key so the input channel has something to record.
    {
        let mut state = fake.inner.lock().unwrap();
        state.latest_features = Some(snapshot(0, 1_000, 220.0));
        state.pending_features = vec![snapshot(0, 1_000, 220.0), snapshot(1, 1_010, 222.0)];
        state.pending_events = vec![CoachEvent::EventsDropped { count: 3 }];
    }
    // Schema 3: the recorder taps the canonical `WindowEvent` stream, not the
    // typed `KeyboardInput` channel, so inject the combined event (as winit
    // would) for it to be captured.
    app.world_mut()
        .write_message(WindowEvent::KeyboardInput(KeyboardInput {
            key_code: KeyCode::F8,
            logical_key: bevy::input::keyboard::Key::Character("8".into()),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        }));
    pump(&mut app);

    // Flush is per-frame in `Last`; one more update guarantees the buffer hit
    // disk after the records above were written.
    app.update();

    let records = read_records(&root);
    assert!(!records.is_empty(), "trace produced no records");

    let kinds: Vec<&str> = records.iter().filter_map(|r| r["k"].as_str()).collect();

    // Header first, exactly once.
    assert_eq!(records[0]["k"], "run", "first line must be the run header");
    assert_eq!(records[0]["schema"], 3);
    assert_eq!(records[0]["replay_of"], Value::Null);

    // Frame records exist (one per update).
    assert!(kinds.contains(&"frame"), "expected per-frame records");

    // The coach channel captured the drained features / event we fed in.
    let coach = records
        .iter()
        .find(|r| r["k"] == "coach")
        .expect("a coach record should capture the fed features/events");
    let drained = coach["drained"].as_array().expect("drained array");
    assert!(
        drained.iter().any(|f| f["hop_index"] == 1),
        "drained features should include hop_index 1, got {drained:?}"
    );
    // Schema 2: the channel carries the raw port snapshot (`f0_hz`), not the
    // head's lifted `Features` (`pitch`). Replay re-runs the lift, so the
    // pre-lift value must survive. The fed snapshot at hop 1 was 222.0 Hz.
    let hop1 = drained
        .iter()
        .find(|f| f["hop_index"] == 1)
        .expect("drained hop_index 1");
    assert!(
        (hop1["f0_hz"].as_f64().expect("raw f0_hz on the snapshot") - 222.0).abs() < 1e-3,
        "coach channel must record raw f0_hz (port type), got {hop1:?}"
    );
    assert!(
        hop1.get("pitch").is_none(),
        "coach channel must NOT carry the lifted `pitch` field, got {hop1:?}"
    );
    assert!(
        coach["events"]
            .as_array()
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "the EventsDropped event should be recorded on the coach channel"
    );

    // The input channel captured the injected keypress.
    let key = records
        .iter()
        .find(|r| r["k"] == "input" && r["input"] == "key")
        .expect("the injected key press should be recorded");
    assert_eq!(key["key"], "F8");
    assert_eq!(key["state"], "pressed");

    let _ = fs::remove_dir_all(&root);
}

/// A graceful exit (`AppExit`) finishes the gzip stream, so the trace carries a
/// valid trailer and a *strict* decoder reads it whole. This is the common
/// case (window close / Cmd-Q); it's what lets stock `gzcat`/`gunzip` read a
/// normally-closed trace with no tolerance tricks. The recovery path for a
/// trailerless (killed) stream is proven separately in `replay::load`'s
/// `truncated_stream_recovers_flushed_lines`.
#[test]
fn graceful_exit_writes_a_valid_gzip_trailer() {
    use std::io::Read;

    let root = temp_root("graceful");
    let (mut app, _fake) = build_recording_app(&root);

    pump(&mut app);
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);

    // Signal a graceful shutdown the way a window-close / Cmd-Q does, then run
    // one more frame so `finish_writer` (in `Last`) sees the `AppExit` message.
    app.world_mut().write_message(AppExit::Success);
    app.update();

    // Strict decoder: `GzDecoder` (not the tolerant `MultiGzDecoder` path the
    // loader uses) errors on a missing trailer. If this reads clean, the
    // trailer is present — exactly what `gzcat` needs.
    let bytes = fs::read(trace::file_path(&root, "run")).expect("trace file exists");
    let mut s = String::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_string(&mut s)
        .expect("a finalized trace decodes cleanly under a strict decoder");
    assert!(
        s.lines()
            .next()
            .map(|l| l.contains("\"run\""))
            .unwrap_or(false),
        "decoded trace should start with the run header, got: {:?}",
        s.lines().next()
    );

    let _ = fs::remove_dir_all(&root);
}
