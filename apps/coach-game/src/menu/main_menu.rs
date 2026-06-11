//! Main menu screen: title + three buttons (Free Practice / Settings / Quit).
//!
//! When `HasPausedSession` is set, the start button reads "Continue"
//! instead of "Free Practice" — the marker (`NewGameButton`) stays the
//! same, since both labels lead to InGame and the OnEnter handler clears
//! the flag either way.

use crate::state::{AppState, HasPausedSession};
use crate::ui::*;
use bevy::prelude::*;

#[derive(Component)]
pub struct NewGameButton;
#[derive(Component)]
pub struct SettingsButton;
#[derive(Component)]
pub struct QuitButton;

pub fn spawn(mut commands: Commands, has_paused: Res<HasPausedSession>) {
    let start_label = if has_paused.0 {
        "Continue"
    } else {
        "Free Practice"
    };
    commands.spawn((
        Name::new("main_menu"),
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
            // Trace paths key off the stable `Name`, not the dynamic label
            // ("Continue" vs "Free Practice"), so an agent reads the same path
            // regardless of session state.
            menu_button("new_game", start_label, NewGameButton),
            menu_button("settings", "Settings", SettingsButton),
            menu_button("quit", "Quit", QuitButton),
        ],
    ));
}

fn menu_button<M: Component>(name: &'static str, label: &str, marker: M) -> impl Bundle {
    (
        Name::new(name),
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
