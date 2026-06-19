//! Integration tests for the state machine + AppCoach seam.
//!
//! Each test spawns the production app schedule via `build_test_app`
//! (no renderer, no real audio), simulates a button click by spawning
//! an entity carrying the relevant marker + `Interaction::Pressed`,
//! pumps `app.update()` twice (see `pump` below for why), then asserts
//! the visible effect (state change, Command recorded on the fake,
//! AppExit written, etc).
//!
//! `world_mut().spawn` (not `Commands`) is used to make the entity
//! visible to systems on the *same* update — a deferred `Commands`
//! spawn would only land at the next sync point and slip past the
//! `Changed<Interaction>` filter.

mod common;

use bevy::prelude::*;
use coach_game::game::PauseButton;
use coach_game::menu::main_menu::{NewGameButton, QuitButton, SettingsButton};
use coach_game::menu::paused::{
    ConfirmCancelButton, ConfirmModalRoot, ConfirmYesButton, PausedSettingsButton,
    QuitToMainButton, ResumeButton, ShowingQuitConfirm,
};
use coach_game::menu::settings::{BackButton, DeviceRow};
use coach_game::state::{AppState, HasPausedSession, SelectedDevice};
use common::{build_test_app, drain_commands, pump};
use domain_ports::app_coach::Command;
use domain_ports::audio_devices::DeviceId;

fn current_state(app: &App) -> AppState {
    *app.world().resource::<State<AppState>>().get()
}

/// Apply the initial OnEnter(MainMenu) and clear the command log so
/// each test's assertions only see commands from its own input.
fn settle(app: &mut App, fake: &common::FakeCoach) {
    pump(app);
    drain_commands(fake);
}

#[test]
fn boots_into_main_menu() {
    let (mut app, _fake) = build_test_app();
    app.update();
    assert_eq!(current_state(&app), AppState::MainMenu);
}

#[test]
fn new_game_transitions_to_in_game_and_starts_session() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::InGame);
    let cmds = drain_commands(&fake);
    // AudioListDevices is now sent by handle_new_game before transitioning; skip it.
    let session_cmds: Vec<_> = cmds
        .iter()
        .filter(|c| !matches!(c, Command::AudioListDevices | Command::AudioPermissionQuery))
        .collect();
    assert!(
        matches!(
            session_cmds.as_slice(),
            [
                Command::MusicConfigureSession { .. },
                Command::AudioStartSession(_)
            ]
        ),
        "expected ConfigureSession then StartSession after entering InGame, got {} commands",
        cmds.len()
    );
}

#[test]
fn start_session_uses_selected_device_id() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    let id = DeviceId("test-mic-7".into());
    app.world_mut().resource_mut::<SelectedDevice>().0 = Some(id.clone());

    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);

    let cmds = drain_commands(&fake);
    // AudioListDevices is now sent by handle_new_game before transitioning; skip it.
    let session_cmds: Vec<_> = cmds
        .iter()
        .filter(|c| !matches!(c, Command::AudioListDevices | Command::AudioPermissionQuery))
        .collect();
    match session_cmds.as_slice() {
        [Command::MusicConfigureSession { .. }, Command::AudioStartSession(cfg)] => {
            assert_eq!(cfg.device_id, Some(id));
        }
        other => panic!(
            "expected ConfigureSession then StartSession with our device id, got {} cmds",
            other.len()
        ),
    }
}

#[test]
fn settings_button_transitions_to_settings_and_lists_devices() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    app.world_mut()
        .spawn((Button, SettingsButton, Interaction::Pressed));
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::Settings);
    let cmds = drain_commands(&fake);
    assert!(
        matches!(cmds.as_slice(), [Command::AudioListDevices]),
        "expected exactly one ListDevices after entering Settings, got {} commands",
        cmds.len()
    );
}

#[test]
fn back_from_settings_returns_to_main_menu() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    // Enter settings.
    app.world_mut()
        .spawn((Button, SettingsButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::Settings);
    drain_commands(&fake);

    // Press Back.
    app.world_mut()
        .spawn((Button, BackButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::MainMenu);
}

/// Enter the Settings state via NextState (bypass the SettingsButton
/// — that's covered by `settings_button_transitions_to_settings_and_lists_devices`).
fn enter_settings(app: &mut App, fake: &common::FakeCoach) {
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::Settings);
    pump(app);
    assert_eq!(current_state(app), AppState::Settings);
    drain_commands(fake);
}

#[test]
fn clicking_device_row_updates_selected_device() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_settings(&mut app, &fake);

    let id = DeviceId("usb-condenser".into());
    app.world_mut()
        .spawn((Button, DeviceRow(Some(id.clone())), Interaction::Pressed));
    pump(&mut app);

    assert_eq!(app.world().resource::<SelectedDevice>().0, Some(id));
}

#[test]
fn clicking_default_row_clears_selected_device() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_settings(&mut app, &fake);

    // Pre-set to something non-default.
    app.world_mut().resource_mut::<SelectedDevice>().0 = Some(DeviceId("old-pick".into()));

    app.world_mut()
        .spawn((Button, DeviceRow(None), Interaction::Pressed));
    pump(&mut app);

    assert_eq!(app.world().resource::<SelectedDevice>().0, None);
}

#[test]
fn exiting_in_game_stops_session() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    // Enter InGame.
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);
    drain_commands(&fake);

    // Force transition out. There's no in-game "menu" button yet, so
    // drive the transition via NextState directly. This still exercises
    // the OnExit(InGame) hook we care about.
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::MainMenu);
    pump(&mut app);

    let cmds = drain_commands(&fake);
    // OnEnter(MainMenu) fires send_boot_permission_query — filter it out.
    let stop_cmds: Vec<_> = cmds
        .iter()
        .filter(|c| !matches!(c, Command::AudioPermissionQuery))
        .collect();
    assert!(
        matches!(stop_cmds.as_slice(), [Command::AudioStopSession]),
        "expected exactly one StopSession after exiting InGame, got {} commands",
        cmds.len()
    );
}

#[test]
fn quit_button_writes_app_exit_and_shuts_down_coach() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    app.world_mut()
        .spawn((Button, QuitButton, Interaction::Pressed));
    pump(&mut app);

    let messages = app
        .world()
        .resource::<bevy::ecs::message::Messages<AppExit>>();
    let mut cursor = messages.get_cursor();
    let exits: Vec<_> = cursor.read(messages).cloned().collect();
    assert_eq!(exits, vec![AppExit::Success]);

    // The Last-schedule shutdown system must have called shutdown on
    // the coach in the same frame, so the AppCoach background thread
    // exits before the runner returns from main.
    assert_eq!(
        fake.inner.lock().unwrap().shutdown_calls,
        1,
        "shutdown should be called exactly once on AppExit"
    );
}

/// Sanity: the static `MinimalPlugins + StatesPlugin` set in our test
/// builder is exactly what `app.update()` needs to drive `OnEnter` and
/// `Update` schedules. If a future Bevy upgrade reorders schedules
/// this test will be the first to fail.
#[test]
fn schedule_ordering_supports_state_transitions_via_pump() {
    let (mut app, _fake) = build_test_app();
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::MainMenu);

    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::Settings);
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::Settings);
}

// --- Pause / Resume / Continue ----------------------------------------

/// Helper: enter InGame from MainMenu, settle, and drain the
/// StartSession command so the test sees only what it triggers.
fn enter_in_game(app: &mut App, fake: &common::FakeCoach) {
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
    assert_eq!(current_state(app), AppState::InGame);
    drain_commands(fake);
}

/// Pressing the on-screen `PauseButton` must do exactly what Escape does:
/// `InGame → Paused` with `HasPausedSession = true`.
#[test]
fn pause_button_transitions_to_paused_and_marks_has_paused() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);

    // Spawn a bare PauseButton entity with Interaction::Pressed.
    // Using world_mut().spawn (not Commands) so it is visible to systems
    // on the same update — the same technique used for all button tests.
    app.world_mut()
        .spawn((Button, PauseButton, Interaction::Pressed));
    pump(&mut app);

    assert_eq!(
        current_state(&app),
        AppState::Paused,
        "pause button should transition to Paused"
    );
    assert!(
        app.world().resource::<HasPausedSession>().0,
        "pause button should set HasPausedSession"
    );
}

/// Helper: drive InGame → Paused via NextState (bypassing the Esc
/// key plumbing — that's covered by the keyboard canary below). Mirrors
/// what `handle_esc_in_game` would do: set `HasPausedSession` and
/// transition. Drains commands so the caller sees only what comes next.
fn enter_paused(app: &mut App, fake: &common::FakeCoach) {
    app.world_mut().resource_mut::<HasPausedSession>().0 = true;
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::Paused);
    pump(app);
    assert_eq!(current_state(app), AppState::Paused);
    drain_commands(fake);
}

#[test]
fn entering_paused_stops_session_via_on_exit_in_game() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);

    app.world_mut().resource_mut::<HasPausedSession>().0 = true;
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::Paused);
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::Paused);
    assert!(app.world().resource::<HasPausedSession>().0);
    let cmds = drain_commands(&fake);
    assert!(
        matches!(cmds.as_slice(), [Command::AudioStopSession]),
        "expected exactly one StopSession on pause, got {} commands",
        cmds.len()
    );
}

#[test]
fn resuming_from_paused_starts_a_fresh_session() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);
    enter_paused(&mut app, &fake);

    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::InGame);
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::InGame);
    assert!(!app.world().resource::<HasPausedSession>().0);
    let cmds = drain_commands(&fake);
    assert!(
        matches!(
            cmds.as_slice(),
            [
                Command::MusicConfigureSession { .. },
                Command::AudioStartSession(_)
            ]
        ),
        "expected ConfigureSession then StartSession on resume, got {} commands",
        cmds.len()
    );
}

#[test]
fn resume_button_in_paused_returns_to_in_game() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);
    enter_paused(&mut app, &fake);

    app.world_mut()
        .spawn((Button, ResumeButton, Interaction::Pressed));
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::InGame);
}

#[test]
fn paused_settings_button_is_disabled_no_transition() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);
    enter_paused(&mut app, &fake);

    // Click the (disabled) Settings button. There's no handler wired
    // for `PausedSettingsButton` at all — the click is a no-op.
    app.world_mut()
        .spawn((Button, PausedSettingsButton, Interaction::Pressed));
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::Paused);
}

#[test]
fn quit_to_main_raises_confirm_then_yes_returns_to_main_menu() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);
    enter_paused(&mut app, &fake);

    // First click: raises confirm modal, stays in Paused.
    app.world_mut()
        .spawn((Button, QuitToMainButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::Paused);
    assert!(app.world().resource::<ShowingQuitConfirm>().0);
    let modal_count = {
        let world = app.world_mut();
        world.query::<&ConfirmModalRoot>().iter(world).count()
    };
    assert_eq!(modal_count, 1, "confirm modal should be on screen");

    // Yes: clears HasPausedSession + transitions to MainMenu.
    app.world_mut()
        .spawn((Button, ConfirmYesButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::MainMenu);
    assert!(!app.world().resource::<HasPausedSession>().0);
    assert!(!app.world().resource::<ShowingQuitConfirm>().0);
}

#[test]
fn quit_confirm_cancel_keeps_session_paused() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);
    enter_paused(&mut app, &fake);

    app.world_mut()
        .spawn((Button, QuitToMainButton, Interaction::Pressed));
    pump(&mut app);
    app.world_mut()
        .spawn((Button, ConfirmCancelButton, Interaction::Pressed));
    pump(&mut app);

    assert_eq!(current_state(&app), AppState::Paused);
    assert!(app.world().resource::<HasPausedSession>().0);
    assert!(!app.world().resource::<ShowingQuitConfirm>().0);
    let modal_count = {
        let world = app.world_mut();
        world.query::<&ConfirmModalRoot>().iter(world).count()
    };
    assert_eq!(
        modal_count, 0,
        "confirm modal should be torn down after Cancel"
    );
}

#[test]
fn continue_button_shown_when_has_paused_session() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);
    enter_in_game(&mut app, &fake);
    enter_paused(&mut app, &fake);
    // Now in Paused with HasPausedSession=true. Drive back to MainMenu
    // *without* clearing the flag (the Quit-to-Main path clears it; an
    // implicit transition does not).
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::MainMenu);
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::MainMenu);
    assert!(app.world().resource::<HasPausedSession>().0);

    // The button text on the NewGameButton entity should now read
    // "Continue". The label is on a child Text node.
    let world = app.world_mut();
    let mut q = world.query_filtered::<&Children, With<NewGameButton>>();
    let children = q.iter(world).next().expect("NewGameButton spawned");
    let child = children[0];
    let text = world.get::<Text>(child).expect("button has Text child");
    assert_eq!(text.0, "Continue");
}

#[test]
fn new_game_button_shown_when_no_paused_session() {
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    let world = app.world_mut();
    let mut q = world.query_filtered::<&Children, With<NewGameButton>>();
    let children = q.iter(world).next().expect("NewGameButton spawned");
    let child = children[0];
    let text = world.get::<Text>(child).expect("button has Text child");
    assert_eq!(text.0, "Free Practice");
}

/// Build a minimal `InputDevice` with the given id + name. Stream
/// metadata is filled with arbitrary-but-valid values — nothing in
/// the test app reads it; only the `persistent_id` matters.
fn fake_device(id: &str, name: &str) -> domain_ports::audio_devices::InputDevice {
    use domain_ports::audio_devices::{
        DeviceId, InputDevice, InputStream, SampleRateSupport, StreamHandle, Transport,
    };
    use std::sync::Arc;
    InputDevice {
        persistent_id: Some(DeviceId(id.into())),
        name: name.into(),
        transport: Transport::Unknown,
        streams: vec![InputStream {
            handle: StreamHandle(Arc::new(())),
            name: name.into(),
            channels: 1,
            sample_rates: SampleRateSupport::ProbeOnly,
        }],
    }
}

#[test]
fn full_device_selection_flow_carries_to_start_session() {
    // End-to-end: Settings opens → fake serves a DevicesListed event →
    // rebuild_device_list spawns real rows → click one of those real
    // rows → Back → New Game → StartSession carries that device id.
    let (mut app, fake) = build_test_app();
    settle(&mut app, &fake);

    // Enter Settings first so on_enter's AudioListDevices auto-response
    // (the fake mic) is consumed by drain_events before we seed our real
    // device list. Seeding before the transition risks the auto-response
    // overwriting our list if drain_events runs after us in the same tick.
    app.world_mut()
        .spawn((Button, SettingsButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::Settings);
    drain_commands(&fake);

    // Now seed the real device list. drain_events will pick it up on the
    // next update and rebuild_device_list will spawn rows for it.
    {
        let mut g = fake.inner.lock().unwrap();
        g.pending_events
            .push(domain_ports::app_coach::CoachEvent::AudioDevicesListed {
                devices: vec![
                    fake_device("usb-condenser", "USB Condenser"),
                    fake_device("airpods", "AirPods Pro"),
                ],
            });
    }
    // Two updates: the first lets drain_events consume the seeded events
    // and mark KnownDevices changed; the second lets rebuild_device_list
    // see that change and spawn rows. (System ordering within a single
    // update is not guaranteed, so two updates make this order-independent.)
    app.update();
    app.update();

    // Sanity: rebuild_device_list spawned a row entity for AirPods
    // with the right persistent_id baked in.
    let airpods_id = DeviceId("airpods".into());
    let row_entity = {
        let world = app.world_mut();
        let mut q = world.query::<(Entity, &DeviceRow)>();
        q.iter(world)
            .find(|(_, row)| row.0.as_ref() == Some(&airpods_id))
            .map(|(e, _)| e)
            .expect("AirPods row not spawned by rebuild_device_list")
    };

    // Click the AirPods row by flipping its Interaction in place
    // (rather than spawning a sibling, so we exercise the real row).
    app.world_mut()
        .entity_mut(row_entity)
        .insert(Interaction::Pressed);
    pump(&mut app);
    assert_eq!(
        app.world().resource::<SelectedDevice>().0,
        Some(airpods_id.clone())
    );

    // Back → MainMenu → New Game.
    app.world_mut()
        .spawn((Button, BackButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::MainMenu);
    drain_commands(&fake);

    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(&mut app);
    assert_eq!(current_state(&app), AppState::InGame);

    let cmds = drain_commands(&fake);
    // AudioListDevices is now sent by handle_new_game before transitioning; skip it.
    let session_cmds: Vec<_> = cmds
        .iter()
        .filter(|c| !matches!(c, Command::AudioListDevices | Command::AudioPermissionQuery))
        .collect();
    match session_cmds.as_slice() {
        [Command::MusicConfigureSession { .. }, Command::AudioStartSession(cfg)] => {
            assert_eq!(cfg.device_id, Some(airpods_id));
        }
        other => panic!(
            "expected ConfigureSession then StartSession with the AirPods id, got {} cmds",
            other.len()
        ),
    }
}
