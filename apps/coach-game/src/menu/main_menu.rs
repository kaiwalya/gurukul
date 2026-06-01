//! Main menu screen: title + three buttons (New Game / Settings / Quit).

use crate::state::AppState;
use crate::ui::*;
use bevy::prelude::*;

/// Marker components on each button so the click handler can tell
/// them apart with a `With<...>` filter instead of a string match.
#[derive(Component)]
pub struct NewGameButton;
#[derive(Component)]
pub struct SettingsButton;
#[derive(Component)]
pub struct QuitButton;

pub fn spawn(mut commands: Commands) {
    commands.spawn((
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
            menu_button("New Game", NewGameButton),
            menu_button("Settings", SettingsButton),
            menu_button("Quit", QuitButton),
        ],
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

pub fn handle_quit(
    q: Query<&Interaction, (Changed<Interaction>, With<QuitButton>)>,
    mut exit: MessageWriter<AppExit>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            exit.write(AppExit::Success);
        }
    }
}
