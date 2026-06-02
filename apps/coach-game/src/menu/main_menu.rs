//! Main menu screen: title + three buttons (New Game / Settings / Quit).
//!
//! When `HasPausedSession` is set, the start button reads "Continue"
//! instead of "New Game" — the marker (`NewGameButton`) stays the same,
//! since both labels lead to InGame and the OnEnter handler clears the
//! flag either way.

use crate::state::{AppState, HasPausedSession};
use crate::ui::*;
use crate::widgets::note_dial::{DialScale, DialSlot, DialState, Needle, NeedleStyle, SlotState};
use bevy::prelude::*;
use std::f32::consts::TAU;

#[derive(Component)]
pub struct NewGameButton;
#[derive(Component)]
pub struct SettingsButton;
#[derive(Component)]
pub struct QuitButton;

pub fn spawn(mut commands: Commands, has_paused: Res<HasPausedSession>) {
    let start_label = if has_paused.0 { "Continue" } else { "New Game" };
    let root = commands
        .spawn((
            DespawnOnExit(AppState::MainMenu),
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: px(24),
                ..default()
            },
            BackgroundColor(COLOR_BG),
            children![
                (
                    Text::new("Gurukul"),
                    TextFont {
                        font_size: FONT_TITLE,
                        ..default()
                    },
                    TextColor(COLOR_ACCENT),
                    Node {
                        margin: UiRect::bottom(px(40)),
                        ..default()
                    },
                ),
                menu_button(start_label, NewGameButton),
                menu_button("Settings", SettingsButton),
                menu_button("Quit", QuitButton),
            ],
        ))
        .id();

    spawn_demo_dial(&mut commands, root);
}

/// Temporary scaffold: drop a 12-equal NoteDial in the lower-right of
/// the main menu so we can eyeball the widget's geometry while we build
/// the real game. Remove once the dial moves to InGame.
fn spawn_demo_dial(commands: &mut Commands, parent: Entity) {
    let slots = (0..12)
        .map(|i| DialSlot {
            angle: i as f32 * TAU / 12.0,
            label: None,
        })
        .collect();

    let states = (0..12).map(|_| SlotState::Lit).collect();

    commands.spawn((
        ChildOf(parent),
        Node {
            position_type: PositionType::Absolute,
            right: px(80),
            bottom: px(80),
            width: px(324),
            height: px(324),
            ..default()
        },
        DialScale { slots },
        DialState {
            slots: states,
            needles: vec![Needle {
                angle: TAU * 0.25,
                style: NeedleStyle::Primary,
            }],
        },
    ));
}

fn menu_button<M: Component>(label: &str, marker: M) -> impl Bundle {
    (
        Button,
        marker,
        Node {
            width: px(240),
            height: px(56),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(COLOR_BUTTON),
        children![(
            Text::new(label),
            TextFont {
                font_size: FONT_BUTTON,
                ..default()
            },
            TextColor(COLOR_TEXT),
        )],
    )
}

pub fn handle_new_game(
    q: Query<&Interaction, (Changed<Interaction>, With<NewGameButton>)>,
    mut next: ResMut<NextState<AppState>>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            next.set(AppState::InGame);
        }
    }
}

pub fn handle_settings(
    q: Query<&Interaction, (Changed<Interaction>, With<SettingsButton>)>,
    mut next: ResMut<NextState<AppState>>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            next.set(AppState::Settings);
        }
    }
}

/// Quit by despawning the primary window rather than writing
/// `AppExit::Success` directly. Bevy 0.18.1 has a known macOS deadlock
/// when AppExit is sent programmatically from a system
/// (bevyengine/bevy#23313): the winit event loop hangs and the window
/// stays open. Despawning `PrimaryWindow` re-enters the native close
/// path, which fires AppExit cleanly via `exit_on_primary_closed`.
///
/// In headless tests there is no PrimaryWindow, so the query is empty
/// and we fall back to writing AppExit directly. The shutdown-on-exit
/// assertion in `quit_button_writes_app_exit_and_shuts_down_coach`
/// covers both paths.
pub fn handle_quit(
    q: Query<&Interaction, (Changed<Interaction>, With<QuitButton>)>,
    window: Query<Entity, With<bevy::window::PrimaryWindow>>,
    mut commands: Commands,
    mut exit: MessageWriter<AppExit>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            if let Ok(w) = window.single() {
                commands.entity(w).despawn();
            } else {
                exit.write(AppExit::Success);
            }
        }
    }
}
