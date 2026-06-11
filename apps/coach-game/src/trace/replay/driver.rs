//! The replay driver: per frame, feed the app exactly what the recorded run saw.
//!
//! One `First` system runs before [`TimeSystems`](bevy::time::TimeSystems) and
//! before the head's reads. For the frame it is serving it:
//! 1. sets [`TimeUpdateStrategy::ManualDuration`] to the recorded `delta_s`, so
//!    Bevy's clock advances by the same step the live run did (this frame's
//!    delta — set before `TimeSystems` reads it, no one-ahead priming needed);
//! 2. loads that frame's recorded `coach` payload into the [`ReplayCoach`],
//!    ready for `coach::drain_events` in `Update`;
//! 3. writes that frame's recorded `input` records back as Bevy messages. They
//!    land before `InputSystems` (PreUpdate) runs, so `ButtonInput<KeyCode>` and
//!    picking update exactly as in the original run.
//!
//! After the last recorded frame it flushes and writes [`AppExit`], unless the
//! run was started with `--hold`.
//!
//! `geom` buckets are dense over `[0, last_frame]` because the recorder writes a
//! `frame` record every frame, so bucket *i* is wall-frame *i* and the cursor is
//! a plain index. The payload is `mem::take`n out of the loaded trace as the
//! cursor advances (each frame served once) — no `CoachEvent` is cloned.

use std::rc::Rc;
use std::time::Duration;

use bevy::app::AppExit;
use bevy::ecs::message::Messages;
use bevy::input::keyboard::{Key, KeyCode, KeyboardInput};
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy::window::{CursorMoved, WindowResized, WindowScaleFactorChanged};

use crate::coach::Coach;

use super::coach::{ReplayCoach, SharedReplayCoach};
use super::load::{InputRecord, LoadedTrace};

/// Driver state: the loaded trace, a cursor into its dense frame buckets, and a
/// shared handle to the same [`ReplayCoach`] inserted as the `Coach`. A
/// `NonSend` resource — it holds the `!Send` `Rc<ReplayCoach>`, mirroring how
/// `RecordingCoach` shares its buffer via a second `Rc` clone.
pub struct ReplayDriver {
    trace: LoadedTrace,
    cursor: usize,
    hold: bool,
    coach: Rc<ReplayCoach>,
}

/// Insert the replay driver + coach and register the driver system. Done as a
/// free fn (not a `Plugin`) because the driver owns the `LoadedTrace` by value
/// and `Plugin::build(&self)` can't move it out. After this the app is ready to
/// `run()`; the caller (`main.rs`) still applies the window override and adds
/// `TracePlugin` so replay records too.
pub fn install(app: &mut App, trace: LoadedTrace, hold: bool) {
    // Prime frame 0's delta before `app.run()` so the very first `First`/
    // `TimeSystems` advance uses the recorded step, not a real-time one.
    let first_delta = trace.frames.first().and_then(|f| f.delta_s).unwrap_or(0.0);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(secs(first_delta)));

    // One `ReplayCoach`, two `Rc` clones: one behind the `Coach(Box<dyn
    // AppCoach>)` handle the head reads through, one in the driver so it can
    // call `load_frame`. The `recording_coach` shared-buffer precedent.
    let coach = Rc::new(ReplayCoach::new());
    app.insert_non_send_resource(Coach(Box::new(SharedReplayCoach(Rc::clone(&coach)))));
    app.insert_non_send_resource(ReplayDriver {
        trace,
        cursor: 0,
        hold,
        coach,
    });

    // Before `TimeSystems` (clock) and before `InputSystems` (PreUpdate reads
    // the messages this writes). `First` is before both.
    app.add_systems(First, drive.before(bevy::time::TimeSystems));
}

/// The six input-message buffers the driver replays through, grouped so
/// [`drive`] stays under the argument-count lint and the channels are named in
/// one place — the write-side mirror of the recorder's `InputReaders`.
///
/// These are `ResMut<Messages<_>>`, not `MessageWriter<_>`, so the driver can
/// **clear** each buffer before injecting (see [`InputWriters::reset`]). A
/// windowed replay is not a closed system: winit drains real OS input into
/// these same buffers before `app.update()` runs, so a stray mouse-move over
/// the replay window would merge into the recorded stream and re-record (and a
/// stray *hover* would flip `Interaction` state, poisoning the geom diff with a
/// divergence the code under test never produced). Clearing first makes the
/// replayed stream the only stream the app sees, whatever the human's hands do.
#[derive(bevy::ecs::system::SystemParam)]
pub struct InputWriters<'w> {
    keys: ResMut<'w, Messages<KeyboardInput>>,
    buttons: ResMut<'w, Messages<MouseButtonInput>>,
    cursors: ResMut<'w, Messages<CursorMoved>>,
    wheels: ResMut<'w, Messages<MouseWheel>>,
    resizes: ResMut<'w, Messages<WindowResized>>,
    scales: ResMut<'w, Messages<WindowScaleFactorChanged>>,
}

impl InputWriters<'_> {
    /// Drop any messages already in this frame's input buffers (real OS events
    /// winit enqueued before the schedule ran) so only the injected, recorded
    /// stream remains. Called once per served frame, before [`inject`].
    fn reset(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.cursors.clear();
        self.wheels.clear();
        self.resizes.clear();
        self.scales.clear();
    }
}

/// Serve one frame, then advance. See the module docs for the per-frame steps.
fn drive(
    mut driver: NonSendMut<ReplayDriver>,
    mut strategy: ResMut<TimeUpdateStrategy>,
    mut writers: InputWriters,
    mut exits: MessageWriter<AppExit>,
) {
    let cursor = driver.cursor;
    if cursor >= driver.trace.frames.len() {
        // Past the last recorded frame. Exit unless holding for a human.
        if !driver.hold {
            exits.write(AppExit::Success);
        }
        return;
    }

    // This frame's delta: `First`'s TimeSystems reads the strategy after us this
    // same frame, so frame N's recorded delta drives frame N.
    let coach = Rc::clone(&driver.coach);
    let frame = &mut driver.trace.frames[cursor];
    if let Some(delta) = frame.delta_s {
        *strategy = TimeUpdateStrategy::ManualDuration(secs(delta));
    }
    if let Some(read) = frame.coach.take() {
        coach.load_frame(read);
    }
    let inputs = std::mem::take(&mut frame.inputs);

    // Suppress any real OS input winit enqueued this frame, then inject the
    // recorded stream. Done unconditionally — a frame with zero recorded inputs
    // must still drop stray live ones, or replay isn't a closed system.
    writers.reset();
    for input in &inputs {
        inject(input, &mut writers);
    }

    driver.cursor += 1;
}

fn secs(s: f32) -> Duration {
    Duration::from_secs_f32(s.max(0.0))
}

/// Turn one recorded [`InputRecord`] back into the Bevy message the recorder
/// read it from. Window is `Entity::PLACEHOLDER` (the messages are consumed by
/// `ButtonInput`/picking, which don't dereference it — the same shortcut the
/// recorder tests take).
fn inject(input: &InputRecord, w: &mut InputWriters) {
    let InputWriters {
        keys,
        buttons,
        cursors,
        wheels,
        resizes,
        scales,
    } = w;
    match input {
        InputRecord::Key { key, state, repeat } => {
            let Some(key_code) = decode_key(key) else {
                return;
            };
            keys.write(KeyboardInput {
                key_code,
                logical_key: Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
                state: decode_state(state),
                text: None,
                repeat: *repeat,
                window: Entity::PLACEHOLDER,
            });
        }
        InputRecord::MouseButton { button, state } => {
            let Some(button) = decode_button(button) else {
                return;
            };
            buttons.write(MouseButtonInput {
                button,
                state: decode_state(state),
                window: Entity::PLACEHOLDER,
            });
        }
        InputRecord::Cursor { pos } => {
            cursors.write(CursorMoved {
                window: Entity::PLACEHOLDER,
                position: Vec2::new(pos[0], pos[1]),
                delta: None,
            });
        }
        InputRecord::Wheel { unit, x, y } => {
            wheels.write(MouseWheel {
                unit: decode_wheel_unit(unit),
                x: *x,
                y: *y,
                window: Entity::PLACEHOLDER,
            });
        }
        InputRecord::Resize { size } => {
            resizes.write(WindowResized {
                window: Entity::PLACEHOLDER,
                width: size[0],
                height: size[1],
            });
        }
        InputRecord::ScaleFactor { scale_factor } => {
            scales.write(WindowScaleFactorChanged {
                window: Entity::PLACEHOLDER,
                scale_factor: *scale_factor,
            });
        }
    }
}

fn decode_state(s: &str) -> ButtonState {
    match s {
        "released" => ButtonState::Released,
        _ => ButtonState::Pressed,
    }
}

fn decode_wheel_unit(s: &str) -> MouseScrollUnit {
    match s {
        "Pixel" => MouseScrollUnit::Pixel,
        _ => MouseScrollUnit::Line,
    }
}

/// Inverse of the recorder's `format!("{:?}", key_code)`. Covers the keys the
/// app actually binds (Esc, Ctrl, F10) plus F8 (used by the recorder tests).
/// An unmapped key is *dropped* — which would make a round-trip diff silently
/// incomplete — so it logs loudly. Extend this table when a new binding lands.
fn decode_key(s: &str) -> Option<KeyCode> {
    Some(match s {
        "Escape" => KeyCode::Escape,
        "ControlLeft" => KeyCode::ControlLeft,
        "ControlRight" => KeyCode::ControlRight,
        "F8" => KeyCode::F8,
        "F10" => KeyCode::F10,
        other => {
            bevy::log::warn!("replay: unmapped key {other:?} — dropping (extend decode_key)");
            return None;
        }
    })
}

fn decode_button(s: &str) -> Option<MouseButton> {
    Some(match s {
        "Left" => MouseButton::Left,
        "Right" => MouseButton::Right,
        "Middle" => MouseButton::Middle,
        other => {
            bevy::log::warn!("replay: unmapped mouse button {other:?} — dropping");
            return None;
        }
    })
}
