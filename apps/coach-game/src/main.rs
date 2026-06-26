//! Production entry point. The real wiring lives in `lib::build_app`;
//! this file just adds the renderer, the real `Coach` construction, and the
//! UX flight recorder (`trace`) — the recorder is wired *here*, not in
//! `build_app`, so headless tests never sprout trace directories.
//!
//! `--replay [path]` re-runs a recorded trace deterministically (no mic, no
//! DSP engine), emitting a fresh trace whose header carries `replay_of`.
//! `path` is a trace file (`traces/<stamp>-ux.jsonl.gz`); omit it to pick the
//! newest automatically. `--hold` keeps the window open after the last
//! replayed frame instead of exiting.
//!
//! `--replay-audio <path>` is a **live run** that swaps the microphone for a
//! WAV file. The engine, worker, UI, and trace recorder all run exactly as in
//! a real session; only the audio source is replaced. Unlike `--replay`, the
//! DSP engine runs — this is the correct tool for visually verifying an engine
//! change against a known recording. When the WAV drains the run is treated as
//! a finished session and returns to the main menu (the WAV adapter models a
//! never-ending mic, so the drain is detected here, app-side; see
//! [`replay_audio`](coach_game::replay_audio)) — start again to replay it.
//!
//! `--autostart` skips the main menu and boots directly into a live session
//! (the same transition the Free Practice button triggers). It is independent
//! of `--replay-audio` — combine them to start a WAV-backed live run without
//! first clicking Free Practice. If `--replay` is also passed, `--autostart`
//! is silently ignored: `--replay` forces its own deterministic flow and never
//! lands on the main menu anyway.
//!
//! `--replay` and `--replay-audio` are mutually exclusive (different execution
//! modes: bypass-engine vs swap-mic). Passing both is an error.

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::{Window, WindowResolution};
use coach_game::trace::replay;
use coach_game::trace::TracePlugin;
use coach_game::{
    add_mesh_trace_plugin, add_mesh_trace_systems, build_app, coach, font, replay_audio, trace,
};

#[derive(Debug, PartialEq)]
struct Args {
    /// `Some` in UX-trace replay mode; the inner option is an explicit trace file path
    /// (`None` ⇒ newest under `traces/`).
    replay: Option<Option<PathBuf>>,
    /// `Some` in WAV-replay live mode; the path is required (no auto-pick).
    replay_audio: Option<PathBuf>,
    /// `--hold`: keep the window after the last replayed frame.
    hold: bool,
    /// `--autostart`: skip the main menu, boot directly into InGame.
    autostart: bool,
}

fn parse_args() -> Args {
    match parse_from(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    }
}

/// Pure arg parser over an iterator of tokens (no `std::env`, no `exit`), so the
/// flag grammar — including the `--replay`/`--replay-audio` interaction — is
/// unit-testable. Returns the message to print before `exit(1)` on a usage error.
fn parse_from(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut replay: Option<Option<PathBuf>> = None;
    let mut replay_audio: Option<PathBuf> = None;
    let mut hold = false;
    let mut autostart = false;
    let mut it = args.peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--replay" => {
                // Optional path follows, unless it's another flag. *Peek* so a
                // following flag (e.g. `--replay --replay-audio x.wav`) is left
                // in the stream to be parsed on its own — consuming it here
                // would silently swallow the flag and bypass mutual exclusion.
                let path = it
                    .peek()
                    .filter(|s| !s.starts_with("--"))
                    .map(PathBuf::from);
                if path.is_some() {
                    it.next();
                }
                replay = Some(path);
            }
            "--replay-audio" => {
                // Path is required.
                match it.peek() {
                    Some(p) if !p.starts_with("--") => {
                        replay_audio = Some(PathBuf::from(it.next().unwrap()));
                    }
                    _ => {
                        return Err("coach-game: --replay-audio requires a path argument".into());
                    }
                }
            }
            "--hold" => hold = true,
            "--autostart" => autostart = true,
            other => eprintln!("coach-game: ignoring unknown arg {other:?}"),
        }
    }
    if replay.is_some() && replay_audio.is_some() {
        return Err(
            "coach-game: --replay and --replay-audio are mutually exclusive\n\
             --replay replays a UX trace (no engine); --replay-audio swaps the mic (engine runs)"
                .into(),
        );
    }
    Ok(Args {
        replay,
        replay_audio,
        hold,
        autostart,
    })
}

fn main() {
    let args = parse_args();
    match args.replay {
        // --autostart is silently ignored in replay mode: the replay driver
        // forces its own deterministic flow and never lands on the main menu.
        Some(path) => run_replay(path, args.hold),
        None => run_live(args.replay_audio, args.autostart),
    }
}

/// Live run: real adapters (or WAV-backed audio if `replay_audio` is set),
/// recording decorator, fresh trace file.
fn run_live(replay_audio: Option<PathBuf>, autostart: bool) {
    // Validate the replay WAV up front. Without this, a bad path panics ~3s
    // deep in Bevy startup (after wgpu/window init) inside the WAV devices
    // adapter, burying the one fact the user needs. Mirror `--replay`'s
    // clean-message-then-exit idiom for missing/bad paths. The adapter owns
    // WAV knowledge — the host just surfaces the error.
    if let Some(wav) = replay_audio.as_ref() {
        if let Err(e) = adapter_audio_wav::probe(wav) {
            eprintln!(
                "coach-game --replay-audio: cannot open {}: {e}",
                wav.display()
            );
            std::process::exit(1);
        }
    }

    let mut app = App::new();
    // GURUKUL_DEVICE_SIZE="w,h,scale" forces a fixed logical size + scale-factor
    // override, to preview the iOS layout on Mac without the simulator. iOS is
    // landscape-locked (see BUILD.md), so width > height — e.g. "852,393,3" for
    // iPhone 15. Documented in PLATFORM-DEBUGGING.md (macOS).
    let mut base_window = Window {
        title: "Gurukul".to_string(),
        ..platform_window()
    };
    if let Ok(spec) = std::env::var("GURUKUL_DEVICE_SIZE") {
        let nums: Vec<f32> = spec
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if let [lw, lh, sf] = nums[..] {
            let sf = sf.max(0.01);
            base_window.resolution = WindowResolution::new((lw * sf) as u32, (lh * sf) as u32)
                .with_scale_factor_override(sf);
            base_window.resizable = false;
            eprintln!("GURUKUL_DEVICE_SIZE: {lw}x{lh} @{sf}x");
        }
    }
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(base_window),
        ..default()
    }));

    // Mint the run stamp first so telemetry and the trace recorder share
    // the same stamp (previously the stamp was born after the coach, so
    // telemetry had no stamp to write to).
    let (stamp, wall_start) = trace::launch_stamp();
    let log_prefix = trace::trace_root().join(&stamp);

    // Build the real coach (or WAV-swapped coach) and wrap it in the recording
    // decorator *before* it becomes the `Coach` handle, then stash the shared
    // trace buffer. The rest of the app still holds a plain `Box<dyn AppCoach>`
    // and is unaware.
    trace::install_recording_coach(
        app.world_mut(),
        coach::build_coach_with_audio(replay_audio.clone(), Some(log_prefix)),
    );

    app.insert_resource(coach_game::game::LaunchStamp(stamp.clone()));
    app.add_plugins(TracePlugin {
        root: trace::trace_root(),
        stamp,
        wall_start,
        replay_of: None,
    });

    if autostart {
        app.insert_resource(coach_game::state::Autostart);
    }

    finish(&mut app);

    // Wire the WAV-end detector only when a WAV was supplied.  These systems
    // must be added AFTER `finish` (which calls `build_app`) so the
    // `FeatureDrainCount` resource they depend on is already registered.
    if replay_audio.is_some() {
        use coach_game::state::AppState;
        app.insert_resource(replay_audio::ReplayAudioEnd::default())
            .add_systems(OnEnter(AppState::InGame), replay_audio::reset_detector)
            .add_systems(
                Update,
                replay_audio::detect_wav_end
                    .after(coach::drain_events)
                    .run_if(bevy::prelude::in_state(AppState::InGame)),
            );
    }

    app.run();
}

/// Replay run: no adapters, no mic. Load the trace, force the window to the
/// recorded frame, drive the clock + inputs + coach from the recording, and
/// record a *new* trace whose header carries `replay_of`.
fn run_replay(explicit: Option<PathBuf>, hold: bool) {
    let root = trace::trace_root();
    let src = match explicit.or_else(|| trace::newest(&root)) {
        Some(p) => p,
        None => {
            eprintln!(
                "coach-game --replay: no trace found under {}",
                root.display()
            );
            std::process::exit(1);
        }
    };

    // Someone with a pre-flat-layout trace may pass a directory path.
    if src.is_dir() {
        eprintln!(
            "coach-game --replay: {:?} is a directory.\n\
             The trace layout is now flat: traces/<stamp>-ux.jsonl.gz.\n\
             Pass the file path directly, e.g. --replay traces/<stamp>-ux.jsonl.gz",
            src
        );
        std::process::exit(1);
    }

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
            title: format!("Gurukul — replay of {}", file_label(&src)),
            resolution,
            ..default()
        }),
        ..default()
    }));

    // Insert the ReplayCoach + driver (clock priming, input injection, coach
    // payload feed, exit-after-last-frame). This also inserts the `Coach`
    // handle, replacing `install_recording_coach`'s job.
    replay::install(&mut app, trace_data, hold);

    // Replay records too: a fresh trace file whose header points back at `src`.
    let (stamp, wall_start) = trace::launch_stamp();
    app.add_plugins(TracePlugin {
        root: root.clone(),
        stamp,
        wall_start,
        replay_of: Some(file_label(&src)),
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
    // Render every frame regardless of window focus. Bevy's default
    // (`WinitSettings::game()`) drops an *unfocused* window into a low-power
    // reactive loop that only wakes on events. Our window opens behind the
    // terminal (unbundled binary, see CLAUDE.md), so it is usually unfocused;
    // as long as audio keeps producing per-frame UI churn the loop stays awake,
    // but the instant the audio source goes quiet — exactly what happens when a
    // `--replay-audio` WAV drains — the loop sleeps and never services the
    // Cmd-Q `AppExit` frame, so `shutdown_on_exit` never runs and the process
    // appears hung. A live coaching session wants continuous rendering anyway.
    app.insert_resource(bevy::winit::WinitSettings {
        focused_mode: bevy::winit::UpdateMode::Continuous,
        unfocused_mode: bevy::winit::UpdateMode::Continuous,
    });

    app.add_systems(Startup, (spawn_camera, font::load));
    // Promote the Devanagari font to the default slot once it loads.
    // Runs every frame until the asset lands, then removes its marker.
    app.add_systems(Update, font::promote_to_default);
    // Register the mesh-trace material plugin and systems here (not in
    // build_app) so headless tests, which use MinimalPlugins and have no
    // Assets<Mesh> / Assets<TraceMaterial>, are unaffected.
    add_mesh_trace_plugin(app);
    build_app(app);
    add_mesh_trace_systems(app);
}

/// The filename of a trace path, for the `replay_of` label and window title.
/// A flat trace path's `file_name()` is the stamped filename — a good
/// `replay_of` label, and directly pasteable as `--replay traces/<that>`.
fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

/// Platform-appropriate base `Window` for the live app. On iOS the OS
/// owns the surface: a default (windowed) `Window` is created at 0×0 and
/// never reflows, so the whole UI lays out into a zero rect and the
/// screen stays blank. `BorderlessFullscreen` makes winit size the
/// window to the screen. On every other platform this is just
/// `Window::default()`, so Mac behaviour is unchanged.
#[cfg(target_os = "ios")]
fn platform_window() -> Window {
    use bevy::window::{MonitorSelection, WindowMode};
    Window {
        mode: WindowMode::BorderlessFullscreen(MonitorSelection::Primary),
        resizable: false,
        ..default()
    }
}

#[cfg(not(target_os = "ios"))]
fn platform_window() -> Window {
    Window::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> Result<Args, String> {
        parse_from(tokens.iter().map(|s| s.to_string()))
    }

    #[test]
    fn replay_audio_alone_is_a_live_swap() {
        let a = parse(&["--replay-audio", "rec.wav"]).unwrap();
        assert!(a.replay.is_none());
        assert_eq!(a.replay_audio, Some(PathBuf::from("rec.wav")));
    }

    #[test]
    fn replay_followed_by_replay_audio_does_not_swallow_the_flag() {
        // Regression: `--replay`'s optional-path peek must NOT consume the
        // following `--replay-audio`. Both flags must register so mutual
        // exclusion fires instead of silently dropping --replay-audio.
        let err = parse(&["--replay", "--replay-audio", "rec.wav"]).unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn replay_with_explicit_path_then_other_flag() {
        let a = parse(&["--replay", "traces/x.gz", "--hold"]).unwrap();
        assert_eq!(a.replay, Some(Some(PathBuf::from("traces/x.gz"))));
        assert!(a.hold);
    }

    #[test]
    fn bare_replay_picks_newest() {
        let a = parse(&["--replay"]).unwrap();
        assert_eq!(a.replay, Some(None));
    }

    #[test]
    fn replay_audio_without_path_is_an_error() {
        let err = parse(&["--replay-audio"]).unwrap_err();
        assert!(err.contains("requires a path"), "got: {err}");
    }

    #[test]
    fn replay_audio_followed_by_flag_is_an_error() {
        // A bare --replay-audio whose "path" is actually the next flag.
        let err = parse(&["--replay-audio", "--hold"]).unwrap_err();
        assert!(err.contains("requires a path"), "got: {err}");
    }
}
