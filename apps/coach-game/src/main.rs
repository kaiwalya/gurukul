//! Production entry point. The real wiring lives in `lib::build_app`;
//! this file just adds the renderer, the real `Coach` construction, and the
//! UX flight recorder (`trace`) — the recorder is wired *here*, not in
//! `build_app`, so headless tests never sprout trace directories.
//!
//! `--replay [dir]` re-runs a recorded trace deterministically (no mic, no DSP
//! engine), emitting a fresh trace whose header carries `replay_of`. `--hold`
//! keeps the window open after the last replayed frame instead of exiting.

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::{Window, WindowResolution};
use coach_game::trace::replay;
use coach_game::trace::TracePlugin;
use coach_game::{build_app, coach, font, trace};

/// Parsed CLI: live run, or replay of a specific/newest trace.
struct Args {
    /// `Some` in replay mode; the inner option is an explicit trace dir
    /// (`None` ⇒ newest under `traces/`).
    replay: Option<Option<PathBuf>>,
    /// `--hold`: keep the window after the last replayed frame.
    hold: bool,
}

fn parse_args() -> Args {
    let mut replay: Option<Option<PathBuf>> = None;
    let mut hold = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--replay" => {
                // Optional path follows, unless it's another flag.
                let next = it.next();
                let dir = next.filter(|s| !s.starts_with("--")).map(PathBuf::from);
                replay = Some(dir);
            }
            "--hold" => hold = true,
            other => eprintln!("coach-game: ignoring unknown arg {other:?}"),
        }
    }
    Args { replay, hold }
}

fn main() {
    let args = parse_args();
    match args.replay {
        Some(dir) => run_replay(dir, args.hold),
        None => run_live(),
    }
}

/// Live run: real adapters, recording decorator, fresh trace dir.
fn run_live() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Gurukul".to_string(),
            ..default()
        }),
        ..default()
    }));

    // Build the real coach and wrap it in the recording decorator *before* it
    // becomes the `Coach` handle, then stash the shared trace buffer. The rest
    // of the app still holds a plain `Box<dyn AppCoach>` and is unaware.
    trace::install_recording_coach(app.world_mut(), coach::build_coach());

    // One trace directory per run, under a gitignored `traces/` next to the
    // working directory. Name + header stamped from wall-clock at launch.
    let (run_dir, wall_start) = trace::launch_stamp();
    app.add_plugins(TracePlugin {
        root: PathBuf::from("traces"),
        run_dir,
        wall_start,
        replay_of: None,
    });

    finish(&mut app);
    app.run();
}

/// Replay run: no adapters, no mic. Load the trace, force the window to the
/// recorded frame, drive the clock + inputs + coach from the recording, and
/// record a *new* trace whose header carries `replay_of`.
fn run_replay(explicit: Option<PathBuf>, hold: bool) {
    let root = PathBuf::from("traces");
    let src = match explicit.or_else(|| replay::newest_dir(&root)) {
        Some(d) => d,
        None => {
            eprintln!(
                "coach-game --replay: no trace found under {}",
                root.display()
            );
            std::process::exit(1);
        }
    };

    let trace_data = match replay::load::load(&src) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("coach-game --replay: {e}");
            std::process::exit(1);
        }
    };

    // Force the window to the recorded logical size + scale factor, so geometry
    // is comparable bit-for-bit even on a different display. `WindowResolution`
    // takes *physical* pixels; physical = logical × scale.
    let [lw, lh] = trace_data.header.window_logical;
    let sf = trace_data.header.scale_factor.max(0.01);
    let (pw, ph) = ((lw * sf) as u32, (lh * sf) as u32);
    let resolution = if pw > 0 && ph > 0 {
        WindowResolution::new(pw, ph).with_scale_factor_override(sf)
    } else {
        // A headless-recorded trace (no window) has zero logical size; fall back
        // to a default window rather than a zero-size one.
        WindowResolution::default().with_scale_factor_override(sf)
    };

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: format!("Gurukul — replay of {}", dir_name(&src)),
            resolution,
            ..default()
        }),
        ..default()
    }));

    // Insert the ReplayCoach + driver (clock priming, input injection, coach
    // payload feed, exit-after-last-frame). This also inserts the `Coach`
    // handle, replacing `install_recording_coach`'s job.
    replay::install(&mut app, trace_data, hold);

    // Replay records too: a fresh trace dir whose header points back at `src`.
    let (run_dir, wall_start) = trace::launch_stamp();
    app.add_plugins(TracePlugin {
        root: root.clone(),
        run_dir,
        wall_start,
        replay_of: Some(dir_name(&src)),
    });
    // The recorder needs the shared trace buffer the `RecordingCoach` would have
    // stashed; in replay there is none, so the `coach` channel records empty
    // reads (the ReplayCoach is not a RecordingCoach). That's correct — the
    // value being verified is the *geom* channel, which records identically.

    finish(&mut app);
    app.run();
}

/// Shared startup wiring (camera, font, game systems) for both modes.
fn finish(app: &mut App) {
    app.add_systems(Startup, (spawn_camera, font::load));
    // Promote the Devanagari font to the default slot once it loads.
    // Runs every frame until the asset lands, then removes its marker.
    app.add_systems(Update, font::promote_to_default);
    build_app(app);
}

/// The trailing directory name of a trace path, for the `replay_of` label and
/// window title.
fn dir_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}
