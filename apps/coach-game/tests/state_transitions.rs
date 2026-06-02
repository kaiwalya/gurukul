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
use coach_game::menu::main_menu::{NewGameButton, QuitButton, SettingsButton};
use coach_game::menu::settings::{BackButton, DeviceRow};
use coach_game::state::{AppState, SelectedDevice};
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
    assert!(
        matches!(cmds.as_slice(), [Command::StartSession(_)]),
        "expected exactly one StartSession after entering InGame, got {} commands",
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
    match cmds.as_slice() {
        [Command::StartSession(cfg)] => {
            assert_eq!(cfg.device_id, Some(id));
        }
        other => panic!(
            "expected StartSession with our device id, got {} cmds",
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
        matches!(cmds.as_slice(), [Command::ListDevices]),
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
    assert!(
        matches!(cmds.as_slice(), [Command::StopSession]),
        "expected exactly one StopSession after exiting InGame, got {} commands",
        cmds.len()
    );
}

#[test]
fn quit_button_writes_app_exit() {
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
