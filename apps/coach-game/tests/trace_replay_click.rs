//! Phase-2 click contract: a replayed click hovers and presses the menu button.
//!
//! This is the test the headless geom round-trip *can't* be: it exercises the
//! one path that broke in practice — a recorded click must, on replay, fire
//! `Interaction::Pressed` on the targeted button and transition the app.
//!
//! The crux the original bug exposed: the menu reads the **legacy `Interaction`**
//! component, which `bevy_ui`'s `ui_focus_system` writes — and that system does
//! *not* read picking or the `CursorMoved` event stream. It takes the cursor
//! from the **`Window` component's `physical_cursor_position`** and the click
//! from `ButtonInput<MouseButton>`. Live, winit sets the window cursor as a
//! side-effect of `CursorMoved`; with winit disabled the driver must mirror that
//! (`set_physical_cursor_position(logical × scale)`), or the button never hovers
//! and the click lands on no node — which is exactly why "Free Practice" never
//! got clicked on replay. So the real contract here is: the driver reproduces
//! winit's *full* per-event side-effect set, not just its messages.
//!
//! This stands up a windowed-but-headless app (a real `Window` + `PrimaryWindow`,
//! a 2× camera, no GPU/winit), feeds it a hand-built trace whose one click lands
//! on "Free Practice" at the button's logical centre, and asserts the state
//! transitions to `InGame`. (`CursorMoved.position` is logical; the button's
//! `UiGlobalTransform.translation` is physical, hence the ÷scale below.) The UI
//! picking backend from `DefaultPlugins` runs in parallel and produces hits too,
//! but the menu's `Interaction` path is what gates the transition.

mod common;

use bevy::camera::{Camera, ComputedCameraValues, RenderTargetInfo};
use bevy::math::UVec2;
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, Window, WindowResolution};
use coach_game::coach::Coach;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::menu::permission::MicStatus;
use coach_game::state::AppState;
use coach_game::trace::replay::{self, load};
use common::FakeCoach;
use domain_ports::audio_driver::AudioInitStatus;

/// The scale factor this harness runs at. Physical = logical × `SCALE`. The
/// window override, camera `RenderTargetInfo`, and the button-centre conversion
/// all read this one constant so they can't drift apart.
const SCALE: f32 = 2.0;

/// Build a picking-enabled headless app: `DefaultPlugins` (no GPU, no winit)
/// brings the UI picking backend; a real `Window` tagged `PrimaryWindow` gives
/// the backend something to normalize against; a hand-set 2× camera drives
/// layout and matches the window as its render target. No `FakeCoach` — replay
/// inserts the `ReplayCoach`.
fn build_picking_app() -> App {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                // We spawn the primary window ourselves (below) so we control
                // its entity and resolution; let the plugin create none.
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

    // A real primary window component (no winit surface). Picking and the camera
    // both resolve `WindowRef::Primary`/`Entity` to this entity.
    app.world_mut().spawn((
        Window {
            resolution: WindowResolution::new(1600, 1200).with_scale_factor_override(SCALE),
            ..default()
        },
        PrimaryWindow,
    ));

    // 2× camera, render target defaulting to `Window(Primary)` — same scale the
    // recording harness uses, so a click position is comparable.
    app.world_mut().spawn((
        Camera2d,
        Camera {
            computed: ComputedCameraValues {
                target_info: Some(RenderTargetInfo {
                    physical_size: UVec2::new(1600, 1200),
                    scale_factor: SCALE,
                }),
                ..default()
            },
            ..default()
        },
    ));

    app
}

/// Drive the app until layout settles and the menu has been laid out.
fn pump_settled(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

/// The **logical**-pixel global centre of the `NewGameButton`, after layout.
/// `WindowEvent::CursorMoved.position` is logical (winit divides physical by the
/// scale factor), and that's what both picking and `ui_focus_system` expect.
/// `UiGlobalTransform.translation` is physical, so we divide by `SCALE`.
fn new_game_center_logical(app: &mut App) -> Vec2 {
    let mut q = app
        .world_mut()
        .query_filtered::<&bevy::ui::UiGlobalTransform, With<NewGameButton>>();
    let xform = q
        .single(app.world())
        .expect("exactly one NewGameButton after layout");
    xform.translation / SCALE
}

/// A `LoadedTrace` of N frames whose frame `click_frame` carries a cursor-move
/// to `pos` followed by a left-button press — the move-then-click order picking
/// depends on. Every frame carries a small fixed delta so the manual clock
/// advances.
fn click_trace(frames: usize, click_frame: u32, pos: Vec2) -> load::LoadedTrace {
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames as u32 {
        let inputs = if f == click_frame {
            vec![
                load::InputRecord::Cursor {
                    pos: [pos.x, pos.y],
                },
                load::InputRecord::MouseButton {
                    button: "Left".to_string(),
                    state: "pressed".to_string(),
                },
            ]
        } else {
            Vec::new()
        };
        out.push(load::FrameRecords {
            frame: f,
            delta_s: Some(0.016),
            coach: None,
            inputs,
        });
    }
    load::LoadedTrace {
        header: load::Header {
            schema: coach_game::trace::SCHEMA_VERSION,
            window_logical: [800.0, 600.0],
            scale_factor: 2.0,
        },
        frames: out,
    }
}

#[test]
fn replayed_click_fires_picking_and_transitions_to_ingame() {
    // First app: lay the menu out and read where "Free Practice" landed. It
    // needs *a* coach (build_app wires the event drain); a FakeCoach suffices —
    // we only read geometry from it.
    let mut probe = build_picking_app();
    probe
        .world_mut()
        .insert_non_send_resource(Coach(Box::new(FakeCoach::default())));
    coach_game::build_app(&mut probe);
    // Pre-grant mic so the button press reaches InGame without a permission modal.
    probe.world_mut().resource_mut::<MicStatus>().0 = AudioInitStatus::Granted;
    pump_settled(&mut probe);
    assert_eq!(
        *probe.world().resource::<State<AppState>>().get(),
        AppState::MainMenu,
        "should start on the main menu"
    );
    let center = new_game_center_logical(&mut probe);

    // Second app: replay a trace whose click targets that centre.
    let mut app = build_picking_app();
    // Click a few frames in, after layout has settled, then run a few more so
    // the press → Interaction::Pressed → state transition can land.
    let trace = click_trace(/* frames */ 10, /* click_frame */ 6, center);
    replay::install(&mut app, trace, /* hold */ true);
    coach_game::build_app(&mut app);
    // Pre-grant mic so the button press reaches InGame without a permission modal.
    app.world_mut().resource_mut::<MicStatus>().0 = AudioInitStatus::Granted;

    // Settle a few frames so the menu (and so the button) is spawned before we
    // attach the observer. The driver serves one recorded frame per update, so
    // after these the click at recorded frame 6 is still ahead.
    app.init_resource::<PickHits>();
    for _ in 0..3 {
        app.update();
    }

    // Observe the picking half *independently* of the legacy `Interaction` path.
    // `Pointer<Over>` fires only when `ui_picking` produces a hit from the
    // replayed cursor — which it can do only by reading the combined
    // `WindowEvent` stream the driver re-emits. The menu's state transition runs
    // off `ui_focus_system` (the Window-cursor component), so without this the
    // entire `WindowEvent`/picking replay half (schema 3's whole reason) would be
    // unasserted: deleting the driver's `w.events.write(..)` lines would leave the
    // transition test green. This counter goes to zero if that regresses.
    {
        let mut q = app
            .world_mut()
            .query_filtered::<Entity, With<NewGameButton>>();
        let button = q
            .single(app.world())
            .expect("one NewGameButton after layout");
        app.world_mut()
            .entity_mut(button)
            .observe(|_: On<Pointer<Over>>, mut hits: ResMut<PickHits>| hits.0 += 1);
    }

    // Pump past the click frame (6) with margin so the press → Interaction::Pressed
    // → state transition can land.
    for _ in 0..14 {
        app.update();
    }

    assert_eq!(
        *app.world().resource::<State<AppState>>().get(),
        AppState::InGame,
        "a replayed click on Free Practice must drive picking and enter the game"
    );

    // The picking path fired: the replayed `WindowEvent` cursor reached
    // `ui_picking` and hit the button. Guards schema 3's combined-stream replay.
    assert!(
        app.world().resource::<PickHits>().0 > 0,
        "the replayed cursor must reach UI picking via the WindowEvent stream \
         (Pointer<Over> on the button) — 0 hits means the combined-stream replay \
         regressed"
    );
}

/// Counts `Pointer<Over>` events the button's observer sees during replay.
#[derive(Resource, Default)]
struct PickHits(u32);
