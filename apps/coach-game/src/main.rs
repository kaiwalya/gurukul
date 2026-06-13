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
//! change against a known recording.
//!
//! `--replay` and `--replay-audio` are mutually exclusive (different execution
//! modes: bypass-engine vs swap-mic). Passing both is an error.

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::{Window, WindowResolution};
use coach_game::trace::replay;
use coach_game::trace::TracePlugin;
use coach_game::{build_app, coach, font, trace};

#[derive(Debug, PartialEq)]
struct Args {
    /// `Some` in UX-trace replay mode; the inner option is an explicit trace file path
    /// (`None` ⇒ newest under `traces/`).
    replay: Option<Option<PathBuf>>,
    /// `Some` in WAV-replay live mode; the path is required (no auto-pick).
    replay_audio: Option<PathBuf>,
    /// `--hold`: keep the window after the last replayed frame.
    hold: bool,
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
    })
}

fn main() {
    let args = parse_args();
    match args.replay {
        Some(path) => run_replay(path, args.hold),
        None => run_live(args.replay_audio),
    }
}

/// Live run: real adapters (or WAV-backed audio if `replay_audio` is set),
/// recording decorator, fresh trace file.
fn run_live(replay_audio: Option<PathBuf>) {
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
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Gurukul".to_string(),
            ..default()
        }),
        ..default()
    }));

    // Build the real coach (or WAV-swapped coach) and wrap it in the recording
    // decorator *before* it becomes the `Coach` handle, then stash the shared
    // trace buffer. The rest of the app still holds a plain `Box<dyn AppCoach>`
    // and is unaware.
    trace::install_recording_coach(app.world_mut(), coach::build_coach_with_audio(replay_audio));

    // One trace file per run, under a gitignored `traces/` next to the
    // working directory. Name + header stamped from wall-clock at launch.
    let (stamp, wall_start) = trace::launch_stamp();
    app.add_plugins(TracePlugin {
        root: PathBuf::from(trace::ROOT),
        stamp,
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
    let root = PathBuf::from(trace::ROOT);
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
    app.add_systems(Startup, (spawn_camera, font::load));
    // Promote the Devanagari font to the default slot once it loads.
    // Runs every frame until the asset lands, then removes its marker.
    app.add_systems(Update, font::promote_to_default);
    build_app(app);
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
