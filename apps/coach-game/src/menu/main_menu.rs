//! Main menu screen: title + three buttons (Free Practice / Settings / Quit).

use crate::menu::permission::{MicStatus, PermissionPrompt};
use crate::state::{AppState, HasPausedSession, KnownDevices};
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::app_coach::Command;
use domain_ports::audio_driver::AudioInitStatus;

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

/// Start/Continue button pressed. Fork on current mic status.
pub fn handle_new_game(
    q: Query<&Interaction, (Changed<Interaction>, With<NewGameButton>)>,
    mic: Res<MicStatus>,
    coach: NonSend<crate::coach::Coach>,
    mut prompt: ResMut<PermissionPrompt>,
) {
    for i in q.iter() {
        if *i != Interaction::Pressed {
            continue;
        }
        match mic.0 {
            AudioInitStatus::Granted => {
                coach.0.send_command(Command::AudioListDevices);
                *prompt = PermissionPrompt::CheckingHardware;
            }
            AudioInitStatus::Undetermined => {
                *prompt = PermissionPrompt::NeedsPermission(AudioInitStatus::Undetermined);
            }
            AudioInitStatus::Denied => {
                *prompt = PermissionPrompt::NeedsPermission(AudioInitStatus::Denied);
            }
        }
    }
}

/// Advance from `CheckingHardware` when a fresh `AudioDevicesListed` reply arrives.
pub fn advance_checking_hardware(
    known: Res<KnownDevices>,
    mut prompt: ResMut<PermissionPrompt>,
    mut next: ResMut<NextState<AppState>>,
) {
    if !known.is_changed() {
        return;
    }
    if *prompt != PermissionPrompt::CheckingHardware {
        return;
    }
    if known.0.is_empty() {
        *prompt = PermissionPrompt::NoHardware;
    } else {
        *prompt = PermissionPrompt::Hidden;
        next.set(AppState::InGame);
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
/// `AppExit::Success` directly.
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
