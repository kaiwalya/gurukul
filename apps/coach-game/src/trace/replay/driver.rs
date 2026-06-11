//! The replay driver: per frame, feed the app exactly what the recorded run saw.
//!
//! One `First` system runs before [`TimeSystems`](bevy::time::TimeSystems) and
//! before the head's reads. For the frame it is serving it:
//! 1. sets [`TimeUpdateStrategy::ManualDuration`] to the recorded `delta_s`, so
//!    Bevy's clock advances by the same step the live run did (this frame's
//!    delta — set before `TimeSystems` reads it, no one-ahead priming needed);
//! 2. loads that frame's recorded `coach` payload into the [`ReplayCoach`],
//!    ready for `coach::drain_events` in `Update`;
//! 3. writes that frame's recorded `input` records back, **replicating winit's
//!    fan-out**: each record becomes a [`WindowEvent`] (the canonical stream UI
//!    picking reads) *and* the matching typed message (`KeyboardInput`,
//!    `CursorMoved`, … — what feeds `ButtonInput`). winit emits both live
//!    (`forward_bevy_events`), and the schema-3 recorder taps the combined
//!    stream, so replay must put both back or picking sees nothing. Window
//!    references are remapped to the live [`PrimaryWindow`] (recorded entity ids
//!    are meaningless on a fresh run; picking hit-tests the window entity, so a
//!    placeholder would never match the camera's render target — the click would
//!    land nowhere). The system runs `.before(PickingSystems::Input)`, so the
//!    events are queued before `mouse_pick_events` reads them this same frame.
//!
//! Each frame the driver first **clears** all input buffers (the combined stream
//! and the typed channels) so live OS input over the replay window can't merge
//! into the recorded stream, *and* re-asserts the replayed cursor onto the
//! `Window` component (the side door the message-clear can't close, since
//! `ui_focus_system` reads the component, not the events). Both happen every
//! frame, recorded inputs or not — replay is a closed system.
//!
//! After the last recorded frame it flushes and writes [`AppExit`], unless the
//! run was started with `--hold`. Even held past the recorded horizon the
//! suppression continues (clear + cursor re-assert), so a human poking the held
//! window can't pollute the trace the recorder is still writing.
//!
//! `geom` buckets are dense over `[0, last_frame]` because the recorder writes a
//! `frame` record every frame, so bucket *i* is wall-frame *i* and the cursor is
//! a plain index. The payload is `mem::take`n out of the loaded trace as the
//! cursor advances (each frame served once) — no `CoachEvent` is cloned.

use std::rc::Rc;
use std::time::Duration;

use bevy::app::AppExit;
use bevy::ecs::message::Messages;
use bevy::input::keyboard::{Key, KeyCode, KeyboardFocusLost, KeyboardInput};
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel};
use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::input::ButtonState;
use bevy::picking::PickingSystems;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy::window::{
    CursorEntered, CursorLeft, CursorMoved, PrimaryWindow, Window, WindowEvent, WindowResized,
    WindowScaleFactorChanged,
};

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
    /// The last replayed cursor position in **logical** px, or `None` if the
    /// cursor never moved or last `left` the window. The driver re-asserts this
    /// onto the `Window` component *every* frame (not just on `Cursor` records),
    /// so a live mouse-move over a windowed replay can't beat the recorded stream
    /// on a frame that carries no cursor record — `ui_focus_system` reads the
    /// component, which `reset()` (a message-only clear) cannot protect. Carried
    /// across the recorded horizon too, so `--hold` stays a closed system.
    replay_cursor: Option<Vec2>,
}

/// Insert the replay driver + coach and register the driver system. Done as a
/// free fn (not a `Plugin`) because the driver owns the `LoadedTrace` by value
/// and `Plugin::build(&self)` can't move it out. After this the app is ready to
/// `run()`; the caller (`main.rs`) still applies the window override and adds
/// `TracePlugin` so replay records too.
pub fn install(app: &mut App, trace: LoadedTrace, hold: bool) {
    // Faithfulness guard. The driver mirrors winit's *cursor* side-effects onto
    // the `Window` component, but NOT a mid-run resize/scale change — those would
    // need `set_physical_resolution`/`set_scale_factor` to keep layout honest,
    // and the replay window is force-set to the recorded frame once at startup
    // (`main.rs`). A recorded resize/scale *after* frame 0 means the live run's
    // window changed mid-flight; replaying it as a bare message (no component
    // mutation) would silently diverge layout. Warn loudly rather than diverge in
    // silence — this matches the recorder's "an unmapped key logs, doesn't vanish"
    // doctrine. (Frame-0 records are the window's own creation echo, benign.)
    let late_window_change = trace
        .frames
        .iter()
        .skip(1)
        .flat_map(|f| f.inputs.iter())
        .filter(|i| {
            matches!(
                i,
                InputRecord::Resize { .. } | InputRecord::ScaleFactor { .. }
            )
        })
        .count();
    if late_window_change > 0 {
        bevy::log::warn!(
            "replay: {late_window_change} resize/scale record(s) after frame 0 — \
             the driver re-emits them as messages but does not mutate the Window \
             component, so layout may diverge from the recorded run"
        );
    }

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
        replay_cursor: None,
    });

    // In `First`, before the consumers that run there or later this frame:
    // - `TimeSystems` — the clock advance, which reads the strategy `drive` sets.
    // - `PickingSystems::Input` (`mouse_pick_events`) — reads the `WindowEvent`s
    //   `drive` writes; without this they'd be read a frame late, or never (next
    //   frame's clear drops them first).
    // - `MessageUpdateSystems` — the message double-buffer swap. Injecting before
    //   it is safe either way (readers scan both buffers), but pinning it keeps
    //   the "injected this frame, read this frame, cleared next frame" lifetime
    //   from resting on an unstated scheduler tie-break a Bevy upgrade could flip.
    app.add_systems(
        First,
        drive
            .before(bevy::time::TimeSystems)
            .before(bevy::ecs::message::MessageUpdateSystems)
            .before(PickingSystems::Input),
    );
}

/// The input-message buffers the driver replays through, plus the live
/// [`PrimaryWindow`] every injected event is stamped with. Grouped so [`drive`]
/// stays under the argument-count lint and the channels live in one place.
///
/// Two tiers of buffer, because the driver replicates winit's fan-out:
/// - `events` — the combined [`WindowEvent`] stream, what UI **picking** reads.
/// - the typed channels (`keys`, `buttons`, `cursors`, …) — what `ButtonInput`
///   and other typed readers consume. winit writes both live; so does the
///   driver, by writing each record to its typed channel *and* `events`.
///
/// All are `ResMut<Messages<_>>`, not `MessageWriter<_>`, so the driver can
/// **clear** each before injecting (see [`InputWriters::reset`]). A windowed
/// replay is not a closed system: winit drains real OS input into these same
/// buffers before `app.update()` runs, so a stray mouse-move over the replay
/// window would merge into the recorded stream and re-record (and a stray
/// *hover* would flip `Interaction` state, poisoning the geom diff with a
/// divergence the code under test never produced). Clearing first makes the
/// replayed stream the only stream the app sees, whatever the human's hands do.
#[derive(bevy::ecs::system::SystemParam)]
pub struct InputWriters<'w, 's> {
    /// The live primary window entity, used to stamp every injected event. The
    /// recorded window id is meaningless on a fresh run; picking hit-tests this
    /// entity against the camera's render target, so it must be the real one.
    primary: Query<'w, 's, Entity, With<PrimaryWindow>>,
    /// The live primary `Window` component. The driver mirrors winit's cursor
    /// side-effect onto it (`set_physical_cursor_position`) — see [`inject`]'s
    /// `Cursor`/`CursorLeft` arms. `bevy_ui`'s legacy `ui_focus_system` (which
    /// writes the `Interaction` the menu reads) takes the cursor from *here*, not
    /// from the `CursorMoved` stream, so re-emitting the event alone never hovers
    /// a button. Winit sets this live; with winit disabled the driver must.
    window_component: Query<'w, 's, &'static mut Window, With<PrimaryWindow>>,
    events: ResMut<'w, Messages<WindowEvent>>,
    keys: ResMut<'w, Messages<KeyboardInput>>,
    focus_lost: ResMut<'w, Messages<KeyboardFocusLost>>,
    buttons: ResMut<'w, Messages<MouseButtonInput>>,
    cursors: ResMut<'w, Messages<CursorMoved>>,
    cursor_entered: ResMut<'w, Messages<CursorEntered>>,
    cursor_left: ResMut<'w, Messages<CursorLeft>>,
    wheels: ResMut<'w, Messages<MouseWheel>>,
    touches: ResMut<'w, Messages<TouchInput>>,
    resizes: ResMut<'w, Messages<WindowResized>>,
    scales: ResMut<'w, Messages<WindowScaleFactorChanged>>,
}

impl InputWriters<'_, '_> {
    /// Drop any messages already in this frame's input buffers (real OS events
    /// winit enqueued before the schedule ran) so only the injected, recorded
    /// stream remains. Clears the combined stream *and* every typed channel —
    /// missing either leaves a side door for live input. Called once per served
    /// frame, before [`inject`].
    ///
    /// Not cleared: `MouseMotion` (raw deltas) — neither recorded nor replayed,
    /// nothing in the app reads it. If a drag-to-pan ever consumes it, add it
    /// here, or it becomes an unguarded live-input side door.
    fn reset(&mut self) {
        self.events.clear();
        self.keys.clear();
        self.focus_lost.clear();
        self.buttons.clear();
        self.cursors.clear();
        self.cursor_entered.clear();
        self.cursor_left.clear();
        self.wheels.clear();
        self.touches.clear();
        self.resizes.clear();
        self.scales.clear();
    }

    /// The live primary window entity, or [`Entity::PLACEHOLDER`] if none exists
    /// (a headless run with no window — then nothing hit-tests it anyway).
    fn window(&self) -> Entity {
        self.primary.single().unwrap_or(Entity::PLACEHOLDER)
    }

    /// Force the `Window` component's physical cursor to mirror the replayed
    /// cursor — `Some(logical × scale)` for a known position, `None` when the
    /// replay cursor is unset/left. Winit sets this live as a side-effect of
    /// `CursorMoved`/`CursorLeft`, and `bevy_ui::ui_focus_system` (which writes
    /// the `Interaction` the menu reads) takes the cursor from *here*, not the
    /// event stream. Called **every** frame so a live mouse-move can't override
    /// the recorded position on a frame that carries no cursor record — the
    /// component side door `reset()`'s message-clear can't close.
    fn enforce_cursor(&mut self, logical: Option<Vec2>) {
        if let Ok(mut win) = self.window_component.single_mut() {
            let scale = win.resolution.scale_factor() as f64;
            win.set_physical_cursor_position(logical.map(|p| p.as_dvec2() * scale));
        }
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
        // Past the last recorded frame. Replay must stay a closed system even
        // here: keep suppressing live OS input (messages *and* the window-cursor
        // component) so a human poking the held window can't pollute the trace
        // the recorder is still writing past the recorded horizon. The frozen
        // final cursor is re-asserted; nothing new is injected.
        writers.reset();
        writers.enforce_cursor(driver.replay_cursor);
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

    // Fold this frame's cursor records into the carried replay-cursor state
    // (last one wins): a `Cursor` sets it, a `CursorLeft` clears it.
    for input in &inputs {
        match input {
            InputRecord::Cursor { pos } => driver.replay_cursor = Some(Vec2::new(pos[0], pos[1])),
            InputRecord::CursorLeft => driver.replay_cursor = None,
            _ => {}
        }
    }

    // Suppress any real OS input winit enqueued this frame (messages), force the
    // window-cursor component to the replayed position (the side door messages
    // can't close), then inject the recorded stream. All unconditional — a frame
    // with zero recorded inputs must still drop stray live input, or replay isn't
    // a closed system.
    writers.reset();
    writers.enforce_cursor(driver.replay_cursor);
    for input in &inputs {
        inject(input, &mut writers);
    }

    driver.cursor += 1;
}

fn secs(s: f32) -> Duration {
    Duration::from_secs_f32(s.max(0.0))
}

/// Replay one recorded [`InputRecord`] as winit would: build the typed Bevy
/// struct stamped with the live primary window, then write it to **both** its
/// typed channel (for `ButtonInput` and other typed readers) and the combined
/// [`WindowEvent`] stream (for UI picking). The two halves mirror winit's
/// `forward_bevy_events`; omitting the combined write is exactly the bug that
/// made replayed clicks invisible to picking.
///
/// An unmapped key/button is dropped (and logged) — see [`decode_key`].
fn inject(input: &InputRecord, w: &mut InputWriters) {
    let window = w.window();
    match input {
        InputRecord::Key { key, state, repeat } => {
            let Some(key_code) = decode_key(key) else {
                return;
            };
            // The recorder keeps only the physical `key_code` (the app binds by
            // code), so `logical_key`/`text` are reconstructed as unidentified.
            // Text entry is therefore NOT replayable through this channel — fine
            // today (nothing types), but a future text input would need the
            // recorder to capture `logical_key`/`text` too.
            let ev = KeyboardInput {
                key_code,
                logical_key: Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
                state: decode_state(state),
                text: None,
                repeat: *repeat,
                window,
            };
            // `KeyboardInput` is not `Copy`, so clone for the second write.
            w.keys.write(ev.clone());
            w.events.write(ev.into());
        }
        InputRecord::KeyboardFocusLost => {
            w.focus_lost.write(KeyboardFocusLost);
            w.events.write(KeyboardFocusLost.into());
        }
        InputRecord::MouseButton { button, state } => {
            let Some(button) = decode_button(button) else {
                return;
            };
            let ev = MouseButtonInput {
                button,
                state: decode_state(state),
                window,
            };
            w.buttons.write(ev);
            w.events.write(ev.into());
        }
        InputRecord::Cursor { pos } => {
            // The window-cursor side-effect winit applies here is mirrored once,
            // unconditionally, by [`drive`]'s `enforce_cursor` (which `ui_focus_
            // system` reads) — `inject` only emits the event halves.
            let ev = CursorMoved {
                window,
                position: Vec2::new(pos[0], pos[1]),
                delta: None,
            };
            // `CursorMoved` is not `Copy`, so clone for the second write.
            w.cursors.write(ev.clone());
            w.events.write(ev.into());
        }
        InputRecord::CursorEntered => {
            w.cursor_entered.write(CursorEntered { window });
            w.events.write(CursorEntered { window }.into());
        }
        InputRecord::CursorLeft => {
            // Window-cursor clear-on-leave is handled by `enforce_cursor`.
            w.cursor_left.write(CursorLeft { window });
            w.events.write(CursorLeft { window }.into());
        }
        InputRecord::Wheel { unit, x, y } => {
            let ev = MouseWheel {
                unit: decode_wheel_unit(unit),
                x: *x,
                y: *y,
                window,
            };
            w.wheels.write(ev);
            w.events.write(ev.into());
        }
        InputRecord::Touch { phase, pos, id } => {
            let ev = TouchInput {
                phase: decode_touch_phase(phase),
                position: Vec2::new(pos[0], pos[1]),
                window,
                force: None,
                id: *id,
            };
            w.touches.write(ev);
            w.events.write(ev.into());
        }
        InputRecord::Resize { size } => {
            let ev = WindowResized {
                window,
                width: size[0],
                height: size[1],
            };
            // `WindowResized` is not `Copy`, so clone for the second write.
            w.resizes.write(ev.clone());
            w.events.write(ev.into());
        }
        InputRecord::ScaleFactor { scale_factor } => {
            let ev = WindowScaleFactorChanged {
                window,
                scale_factor: *scale_factor,
            };
            // `WindowScaleFactorChanged` is not `Copy`, so clone for the second.
            w.scales.write(ev.clone());
            w.events.write(ev.into());
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

fn decode_touch_phase(s: &str) -> TouchPhase {
    match s {
        "Started" => TouchPhase::Started,
        "Moved" => TouchPhase::Moved,
        "Ended" => TouchPhase::Ended,
        "Canceled" => TouchPhase::Canceled,
        // The recorder only ever writes the four phases above; anything else is a
        // corrupt/forward-version trace. Default to `Moved` but log, matching the
        // log-and-don't-silently-coerce doctrine `decode_key` follows.
        other => {
            bevy::log::warn!("replay: unknown touch phase {other:?} — treating as Moved");
            TouchPhase::Moved
        }
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
