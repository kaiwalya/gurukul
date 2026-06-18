//! Mic-permission UX: the `PermissionPrompt` resource, its modal sync
//! system, and the stateless content helper `spawn_permission_panel`.

use crate::state::AppState;
use crate::ui::{self, *};
use bevy::prelude::*;
use domain_ports::app_coach::Command;
use domain_ports::audio_driver::AudioInitStatus;

// ── Resource ─────────────────────────────────────────────────────────────

/// State machine for the permission UX. Changed<> drives
/// `sync_permission_modal`.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
pub enum PermissionPrompt {
    #[default]
    Hidden,
    NeedsPermission(AudioInitStatus),
    RequestPending,
    CheckingHardware,
    NoHardware,
}

// ── Head mic-status ───────────────────────────────────────────────────────

/// Head-side read model for the current OS mic permission.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicStatus(pub AudioInitStatus);

impl Default for MicStatus {
    fn default() -> Self {
        Self(AudioInitStatus::Undetermined)
    }
}

// ── Marker components ─────────────────────────────────────────────────────

#[derive(Component)]
pub struct PermissionModalRoot;

#[derive(Component)]
pub struct AllowMicButton;

#[derive(Component)]
pub struct OpenSettingsButton;

#[derive(Component)]
pub struct PermissionCancelButton;

// ── Content helper ────────────────────────────────────────────────────────

/// Stateless content helper: spawn headline + body copy + action button
/// as children of `parent`.
pub fn spawn_permission_panel(commands: &mut Commands, parent: Entity, status: AudioInitStatus) {
    let (headline, body, btn_label) = copy_for_status(status);
    commands.spawn((
        ChildOf(parent),
        Text::new(headline),
        TextFont {
            font_size: FONT_HEADER,
            ..default()
        },
        TextColor(COLOR_TEXT),
        Node {
            margin: UiRect::bottom(px(8)),
            ..default()
        },
    ));
    commands.spawn((
        ChildOf(parent),
        Text::new(body),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT_DIM),
        Node {
            margin: UiRect::bottom(px(16)),
            ..default()
        },
    ));
    match status {
        AudioInitStatus::Undetermined => {
            spawn_action_button(commands, parent, btn_label, AllowMicButton, false);
        }
        AudioInitStatus::Denied => {
            spawn_action_button(commands, parent, btn_label, OpenSettingsButton, false);
        }
        AudioInitStatus::Granted => {}
    }
}

pub fn copy_for_status(status: AudioInitStatus) -> (&'static str, &'static str, &'static str) {
    match status {
        AudioInitStatus::Undetermined => (
            "Microphone needed",
            "Gurukul listens to you sing.",
            "Allow Microphone",
        ),
        AudioInitStatus::Denied => (
            "Microphone is off",
            "Turn it on in Settings to sing.",
            "Open Settings",
        ),
        AudioInitStatus::Granted => ("", "", ""),
    }
}

pub fn spawn_action_button<M: Component>(
    commands: &mut Commands,
    parent: Entity,
    label: &'static str,
    marker: M,
    disabled: bool,
) {
    let (bg, text_color) = if disabled {
        (COLOR_BUTTON_DISABLED, COLOR_TEXT_DIM)
    } else {
        (COLOR_BUTTON, COLOR_TEXT)
    };
    let btn = commands
        .spawn((
            ChildOf(parent),
            Button,
            marker,
            Node {
                width: px(200),
                height: px(56),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(bg),
        ))
        .id();
    if disabled {
        commands.entity(btn).insert(ButtonDisabled);
    }
    commands.entity(btn).with_child((
        Text::new(label),
        TextFont {
            font_size: FONT_BUTTON,
            ..default()
        },
        TextColor(text_color),
    ));
}

// ── Modal sync ────────────────────────────────────────────────────────────

pub fn sync_permission_modal(
    mut commands: Commands,
    prompt: Res<PermissionPrompt>,
    existing: Query<Entity, With<PermissionModalRoot>>,
) {
    if !prompt.is_changed() {
        return;
    }

    for e in existing.iter() {
        commands.entity(e).despawn();
    }

    match prompt.as_ref() {
        PermissionPrompt::Hidden | PermissionPrompt::CheckingHardware => {}
        PermissionPrompt::NeedsPermission(status) => {
            let status = *status;
            let backdrop = commands
                .spawn((
                    Name::new("permission_modal"),
                    PermissionModalRoot,
                    DespawnOnExit(AppState::MainMenu),
                    Node {
                        width: percent(100),
                        height: percent(100),
                        position_type: PositionType::Absolute,
                        flex_direction: FlexDirection::Column,
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::Center,
                        row_gap: px(16),
                        ..default()
                    },
                    BackgroundColor(COLOR_OVERLAY),
                ))
                .id();
            spawn_permission_panel(&mut commands, backdrop, status);
            spawn_action_button(
                &mut commands,
                backdrop,
                "Cancel",
                PermissionCancelButton,
                false,
            );
        }
        PermissionPrompt::RequestPending => {
            let backdrop = ui::spawn_overlay_modal(
                &mut commands,
                "Microphone needed",
                Some("Gurukul listens to you sing."),
                vec![ui::ModalButton {
                    label: "Waiting\u{2026}",
                    marker: AllowMicButton,
                    disabled: true,
                }],
            );
            commands.entity(backdrop).insert((
                Name::new("permission_modal"),
                PermissionModalRoot,
                DespawnOnExit(AppState::MainMenu),
            ));
        }
        PermissionPrompt::NoHardware => {
            let backdrop = ui::spawn_overlay_modal(
                &mut commands,
                "No microphone found.",
                Some("Connect a microphone and try again."),
                vec![ui::ModalButton {
                    label: "Cancel",
                    marker: PermissionCancelButton,
                    disabled: false,
                }],
            );
            commands.entity(backdrop).insert((
                Name::new("permission_modal"),
                PermissionModalRoot,
                DespawnOnExit(AppState::MainMenu),
            ));
        }
    }
}

// ── Button handlers ───────────────────────────────────────────────────────

pub fn handle_allow_mic(
    q: Query<
        &Interaction,
        (
            Changed<Interaction>,
            With<AllowMicButton>,
            Without<ButtonDisabled>,
        ),
    >,
    coach: NonSend<crate::coach::Coach>,
    mut prompt: ResMut<PermissionPrompt>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            coach.0.send_command(Command::AudioPermissionRequest);
            *prompt = PermissionPrompt::RequestPending;
        }
    }
}

pub fn handle_open_settings(
    q: Query<&Interaction, (Changed<Interaction>, With<OpenSettingsButton>)>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            open_os_settings();
        }
    }
}

pub fn handle_permission_cancel(
    q: Query<&Interaction, (Changed<Interaction>, With<PermissionCancelButton>)>,
    mut prompt: ResMut<PermissionPrompt>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            *prompt = PermissionPrompt::Hidden;
        }
    }
}

// ── iOS "Open Settings" ───────────────────────────────────────────────────

#[cfg(target_os = "ios")]
fn open_os_settings() {
    info!("TODO(1.6.2b): open iOS Settings URL");
}

#[cfg(not(target_os = "ios"))]
fn open_os_settings() {}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_for_undetermined() {
        let (h, b, btn) = copy_for_status(AudioInitStatus::Undetermined);
        assert_eq!(h, "Microphone needed");
        assert!(!b.is_empty());
        assert_eq!(btn, "Allow Microphone");
    }

    #[test]
    fn copy_for_denied() {
        let (h, b, btn) = copy_for_status(AudioInitStatus::Denied);
        assert_eq!(h, "Microphone is off");
        assert!(!b.is_empty());
        assert_eq!(btn, "Open Settings");
    }

    #[test]
    fn prompt_default_is_hidden() {
        let p = PermissionPrompt::default();
        assert_eq!(p, PermissionPrompt::Hidden);
    }

    #[test]
    fn mic_status_default_is_undetermined() {
        let m = MicStatus::default();
        assert_eq!(m.0, AudioInitStatus::Undetermined);
    }
}
