//! Headless ECS coverage for the InGame widget trees: dial shell, HUD
//! badge/rows, and the scale-picker overlay. Asserts entity counts, marker
//! components, scene contracts, and the re-entry repaint.

mod common;

use bevy::prelude::*;
use coach_game::game::scale_picker::ShowingScalePicker;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::state::AppState;
use coach_game::widgets::hud::{HudBadge, HudDegRow, HudSceneRes};
use coach_game::widgets::note_dial::{DialHub, DialHubLabel, DialScale, DialState, NoteDialRoot};
use coach_game::widgets::scale_picker::{
    ScalePickerCloseButton, ScalePickerRoot, ScalePickerRows, ScaleRow,
};
use common::{build_test_app, pump, FakeCoach};
use domain_ports::app_coach::{CoachEvent, MusicInfo};
use domain_ports::pitch::PitchLog2Interval;
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

fn music(octave: i32) -> MusicInfo {
    let tuning = TuningAbsolute::new(TuningKind::TwelveTet.intervals(), PitchLog2Interval(0.17));
    MusicInfo {
        scale: Scale::new(
            ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
            tuning.shift_up(3),
            octave,
        ),
    }
}

fn publish_music(fake: &FakeCoach, info: MusicInfo) {
    let mut state = fake.inner.lock().unwrap();
    state.music_info = Some(info);
    state
        .pending_events
        .push(CoachEvent::MusicSessionConfigured { scale: info.scale });
}

fn enter_in_game(app: &mut App) {
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
    assert_eq!(
        *app.world().resource::<State<AppState>>().get(),
        AppState::InGame
    );
}

fn count<C: Component>(app: &mut App) -> usize {
    let world = app.world_mut();
    world
        .query_filtered::<Entity, With<C>>()
        .iter(world)
        .count()
}

// --- dial shell -------------------------------------------------------

#[test]
fn dial_shell_has_hub_markers_and_scene_components() {
    let (mut app, _fake) = build_test_app();
    pump(&mut app);
    enter_in_game(&mut app);

    assert_eq!(count::<NoteDialRoot>(&mut app), 1, "one dial shell");
    assert_eq!(count::<DialHub>(&mut app), 1, "one hub");
    assert_eq!(count::<DialHubLabel>(&mut app), 1, "one hub label");

    // The scene contract lives on the shell entity.
    let world = app.world_mut();
    let with_scene = world
        .query_filtered::<Entity, (With<NoteDialRoot>, With<DialScale>, With<DialState>)>()
        .iter(world)
        .count();
    assert_eq!(with_scene, 1, "shell carries DialScale + DialState");
}

// --- HUD --------------------------------------------------------------

#[test]
fn hud_has_badge_and_row_and_paints_from_scene() {
    let (mut app, fake) = build_test_app();
    pump(&mut app);
    publish_music(&fake, music(8));
    enter_in_game(&mut app);
    // One more frame so refresh/sync_text run after the snapshot lands.
    app.update();

    assert_eq!(count::<HudBadge>(&mut app), 1, "one badge");
    assert_eq!(count::<HudDegRow>(&mut app), 1, "one row");

    let scene_row = app.world().resource::<HudSceneRes>().deg_row.clone();
    assert_eq!(scene_row, "int 2 2 1 2 2 2 1", "scene projected from music");

    let world = app.world_mut();
    let row_text = world
        .query_filtered::<&Text, With<HudDegRow>>()
        .single(world)
        .map(|t| t.as_str().to_string())
        .unwrap();
    assert_eq!(row_text, scene_row, "row text synced from scene");
}

#[test]
fn hud_repaints_on_reentry_with_unchanged_music() {
    let (mut app, fake) = build_test_app();
    pump(&mut app);
    publish_music(&fake, music(8));
    enter_in_game(&mut app);
    app.update();

    // Leave InGame (despawns the HUD tree) and re-enter with the SAME music
    // info — no MusicSessionConfigured event, so MusicInfoRes is unchanged.
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::MainMenu);
    pump(&mut app);
    enter_in_game(&mut app);
    app.update();

    // The freshly-spawned row must still show the row, not the empty
    // placeholder it spawned with.
    let world = app.world_mut();
    let row_text = world
        .query_filtered::<&Text, With<HudDegRow>>()
        .single(world)
        .map(|t| t.as_str().to_string())
        .unwrap();
    assert_eq!(
        row_text, "int 2 2 1 2 2 2 1",
        "re-entry must repaint the HUD even with unchanged music info"
    );
}

// --- scale picker -----------------------------------------------------

fn open_picker_with(app: &mut App, fake: &FakeCoach, shapes: Vec<ScaleIntervals>) {
    // Press the HUD badge → handle_hud_click sends MusicListScales, opens picker.
    let badge = {
        let world = app.world_mut();
        world
            .query_filtered::<Entity, With<HudBadge>>()
            .single(world)
            .unwrap()
    };
    app.world_mut()
        .entity_mut(badge)
        .insert(Interaction::Pressed);
    // Queue the catalogue reply for the next drain_events.
    fake.inner
        .lock()
        .unwrap()
        .pending_events
        .push(CoachEvent::MusicScalesListed { shapes });
    pump(app);
}

#[test]
fn scale_picker_opens_repopulates_and_closes() {
    let (mut app, fake) = build_test_app();
    pump(&mut app);
    publish_music(&fake, music(8));
    enter_in_game(&mut app);
    app.update();

    let two = vec![
        ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
        ScaleIntervals::from_widths(&[2, 1, 2, 2, 1, 2, 2]),
    ];
    open_picker_with(&mut app, &fake, two.clone());

    assert!(app.world().resource::<ShowingScalePicker>().0, "open flag");
    assert_eq!(count::<ScalePickerRoot>(&mut app), 1, "one overlay root");
    assert_eq!(count::<ScalePickerRows>(&mut app), 1, "one rows container");
    assert_eq!(
        count::<ScalePickerCloseButton>(&mut app),
        1,
        "one close btn"
    );
    assert_eq!(count::<ScaleRow>(&mut app), two.len(), "one row per shape");

    // A fresh catalogue arriving while open repopulates the rows.
    fake.inner
        .lock()
        .unwrap()
        .pending_events
        .push(CoachEvent::MusicScalesListed {
            shapes: vec![ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1])],
        });
    app.update();
    assert_eq!(
        count::<ScaleRow>(&mut app),
        1,
        "rows repopulated from new catalogue"
    );

    // Close via the close button.
    let close = {
        let world = app.world_mut();
        world
            .query_filtered::<Entity, With<ScalePickerCloseButton>>()
            .single(world)
            .unwrap()
    };
    app.world_mut()
        .entity_mut(close)
        .insert(Interaction::Pressed);
    pump(&mut app);

    assert!(
        !app.world().resource::<ShowingScalePicker>().0,
        "closed flag"
    );
    assert_eq!(count::<ScalePickerRoot>(&mut app), 0, "overlay despawned");
}
