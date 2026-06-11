//! UX flight recorder: a per-run JSONL trace of what the app *saw* (inputs,
//! coach reads, clock) and what it *did on screen* (computed geometry), so an
//! agent can debug rendering bugs from data instead of a human's description.
//!
//! A non-slice crate-level module (like [`coach`](crate::coach)). Wired in
//! `main.rs`, **not** `build_app` — headless tests must not sprout trace
//! directories; a test that wants the recorder adds [`TracePlugin`] explicitly
//! with a temp dir.
//!
//! Two halves (see `docs/COACH_GAME_UX_TRACE_PLAN.md`):
//! 1. Port-side [`RecordingCoach`] decorator + shared [`TraceBuffer`].
//! 2. Bevy-side recording [`systems`] writing into a [`TraceWriter`].
//!
//! Doctrine: recording computed pixels is pure observability output, not a
//! decision input — the same exemption telemetry has from the pixel-direction
//! rule (`ARCHITECTURE.md`).

mod record;
mod recording_coach;
mod systems;
mod wallclock;
mod writer;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use bevy::diagnostic::FrameCountPlugin;
use bevy::prelude::*;
use bevy::ui::UiSystems;

use crate::coach::Coach;
use domain_ports::app_coach::AppCoach;

pub use record::SCHEMA_VERSION;
pub use recording_coach::{RecordingCoach, TraceBuffer, TraceBufferHandle};
pub use wallclock::launch_stamp;
pub use writer::TraceWriter;

use record::Body;
use systems::{GeomMemory, MarkerCounter};

/// Wrap a real coach in a [`RecordingCoach`], insert it as the `NonSend`
/// [`Coach`] handle, and stash the shared [`TraceBufferHandle`] for the
/// recording systems. Replaces `coach::spawn_coach` in a recording build:
/// `main.rs` builds the adapter, hands it here, and the rest of the app is
/// none the wiser (it still holds a `Box<dyn AppCoach>`).
pub fn install_recording_coach(world: &mut World, inner: Box<dyn AppCoach>) {
    let buffer: TraceBufferHandle = Rc::new(RefCell::new(TraceBuffer::default()));
    let coach = RecordingCoach::new(inner, Rc::clone(&buffer));
    world.insert_non_send_resource(Coach(Box::new(coach)));
    world.insert_non_send_resource(buffer);
}

/// Wrap the **already-inserted** [`Coach`] handle in a [`RecordingCoach`] and
/// stash the shared buffer. For an app whose coach was wired by something else
/// (a test harness that inserts a `FakeCoach`, say): take the handle out,
/// decorate it, put it back. Equivalent to [`install_recording_coach`] but for
/// the "coach already present" case.
///
/// Panics if no [`Coach`] is present — the caller must wire one first.
pub fn install_recording_coach_over_existing(world: &mut World) {
    let existing = world
        .remove_non_send_resource::<Coach>()
        .expect("a Coach must be inserted before wrapping it for recording");
    install_recording_coach(world, existing.0);
}

/// The recording plugin. Adds the writer, the per-frame recording systems, and
/// the marker/geom state. Expects the [`TraceBufferHandle`] to already be
/// inserted (by [`install_recording_coach`]); a [`Coach`] that is not a
/// `RecordingCoach` simply yields empty `coach` records.
pub struct TracePlugin {
    /// Root directory traces are written under (gitignored `traces/` in
    /// production; a temp dir in tests).
    pub root: PathBuf,
    /// Launch-time run directory name, lexicographically sortable. The caller
    /// stamps it (production: UTC wall-clock; tests: a fixed name) so the
    /// module needs no clock of its own.
    pub run_dir: String,
    /// Header `wall_start` string for the `run` record.
    pub wall_start: String,
    /// `replay_of` for the `run` header (replay runs only; `None` for live).
    pub replay_of: Option<String>,
}

impl Plugin for TracePlugin {
    fn build(&self, app: &mut App) {
        let writer = match TraceWriter::create(&self.root, &self.run_dir) {
            Ok(w) => w,
            Err(e) => {
                bevy::log::error!(
                    "trace: could not open {:?}/{}: {e}",
                    self.root,
                    self.run_dir
                );
                return;
            }
        };
        bevy::log::info!("trace: recording to {:?}", writer.dir());

        // `FrameCount` drives the `f` field; ensure its plugin is present
        // (DefaultPlugins includes it, but a MinimalPlugins test may not).
        if !app.is_plugin_added::<FrameCountPlugin>() {
            app.add_plugins(FrameCountPlugin);
        }

        app.insert_resource(writer)
            .init_resource::<MarkerCounter>()
            .init_resource::<GeomMemory>();

        // The window-side input messages (`CursorMoved`, `WindowResized`,
        // `WindowScaleFactorChanged`) are registered by `WindowPlugin` in a
        // full app but absent under `MinimalPlugins`. The recorder reads them
        // either way, so ensure the channels exist — `add_message` is
        // idempotent when the host already added them.
        app.add_message::<bevy::window::CursorMoved>()
            .add_message::<bevy::window::WindowResized>()
            .add_message::<bevy::window::WindowScaleFactorChanged>();

        // The `run` header: first line, written once at startup before any
        // per-frame record. A Startup system guarantees it precedes frame 0.
        let header = RunHeader {
            wall_start: self.wall_start.clone(),
            replay_of: self.replay_of.clone(),
        };
        app.insert_resource(header);
        app.add_systems(Startup, write_run_header);

        // `delta_s` must be this frame's delta, not the previous one: order
        // after Bevy's clock advance (also in `First`), or the record can
        // nondeterministically capture a stale delta.
        app.add_systems(First, systems::record_frame.after(bevy::time::TimeSystems));
        // Chain the Update recorders so their lines land in a deterministic
        // order within the frame (input → mark → state → coach), making
        // run-to-run diffs stable. `record_coach` still runs after the single
        // coach reader, so the buffer is full when it drains.
        app.add_systems(
            Update,
            (
                systems::record_inputs,
                systems::record_marks,
                systems::record_state,
                systems::record_coach.after(crate::coach::drain_events),
            )
                .chain(),
        );
        app.add_systems(
            PostUpdate,
            systems::record_geom.after(UiSystems::PostLayout),
        );
        app.add_systems(Last, systems::flush_writer);
    }
}

/// Header fields captured at plugin build, consumed once by [`write_run_header`].
#[derive(Resource)]
struct RunHeader {
    wall_start: String,
    replay_of: Option<String>,
}

/// Write the one-time `run` record. Reads the primary window's logical size +
/// scale factor if a window exists (a headless test has none — then it falls
/// back to zeros, still a valid header).
fn write_run_header(
    mut writer: ResMut<TraceWriter>,
    header: Res<RunHeader>,
    windows: Query<&Window>,
) {
    let (window_logical, scale_factor) = windows
        .iter()
        .next()
        .map(|w| ([w.width(), w.height()], w.scale_factor()))
        .unwrap_or(([0.0, 0.0], 1.0));
    writer.write(
        0,
        Body::Run {
            schema: SCHEMA_VERSION,
            app_version: env!("CARGO_PKG_VERSION"),
            window_logical,
            scale_factor,
            wall_start: header.wall_start.clone(),
            replay_of: header.replay_of.clone(),
        },
    );
}
