//! Paused overlay: Resume / Settings (disabled) / Quit to Main Menu.
//!
//! Drawn full-screen on top of the (now-stopped) game. Settings is
//! intentionally disabled here — like a disconnected controller, you
//! can't change input devices mid-run. To re-configure audio, quit to
//! the main menu first.

use crate::state::AppState;
use crate::ui::*;
use bevy::prelude::*;

#[derive(Component)]
pub struct ResumeButton;
#[derive(Component)]
pub struct PausedSettingsButton;
#[derive(Component)]
pub struct QuitToMainButton;

/// True while the "Quit? Your session will end." confirm modal is on
/// screen. Reset on enter/exit of Paused so we never leak it across
/// pauses.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ShowingQuitConfirm(pub bool);

#[derive(Component)]
pub struct ConfirmModalRoot;
#[derive(Component)]
pub struct ConfirmYesButton;
#[derive(Component)]
pub struct ConfirmCancelButton;

pub fn spawn(mut commands: Commands) {
    let root = commands
        .spawn((
            DespawnOnExit(AppState::Paused),
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: px(24),
                ..default()
            },
            BackgroundColor(COLOR_OVERLAY),
        ))
        .id();

    commands.entity(root).with_children(|parent| {
        parent.spawn((
            Text::new("Paused"),
            TextFont {
                font_size: FONT_TITLE,
                ..default()
            },
            TextColor(COLOR_ACCENT),
            Node {
                margin: UiRect::bottom(px(40)),
                ..default()
            },
        ));
        spawn_button(parent, "Resume", ResumeButton, false);
        spawn_button(parent, "Settings", PausedSettingsButton, true);
        spawn_button(parent, "Quit to Main Menu", QuitToMainButton, false);
    });
}

fn spawn_button<M: Component>(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    marker: M,
    disabled: bool,
) {
    let (bg, text_color) = if disabled {
        (COLOR_BUTTON_DISABLED, COLOR_TEXT_DIM)
    } else {
        (COLOR_BUTTON, COLOR_TEXT)
    };
    let mut btn = parent.spawn((
        Button,
        marker,
        Node {
            width: px(280),
            height: px(56),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(bg),
    ));
    if disabled {
        btn.insert(ButtonDisabled);
    }
    btn.with_child((
        Text::new(label),
        TextFont {
            font_size: FONT_BUTTON,
            ..default()
        },
        TextColor(text_color),
    ));
}

pub fn handle_resume(
    q: Query<
        &Interaction,
        (
            Changed<Interaction>,
            With<ResumeButton>,
            Without<ButtonDisabled>,
        ),
    >,
    mut next: ResMut<NextState<AppState>>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            next.set(AppState::InGame);
        }
    }
}

/// "Quit to Main Menu" doesn't quit directly — it raises the confirm
/// modal. Actual transition happens in `handle_confirm_yes`.
pub fn handle_quit_to_main(
    q: Query<
        &Interaction,
        (
            Changed<Interaction>,
            With<QuitToMainButton>,
            Without<ButtonDisabled>,
        ),
    >,
    mut showing: ResMut<ShowingQuitConfirm>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            showing.0 = true;
        }
    }
}

pub fn handle_confirm_yes(
    q: Query<&Interaction, (Changed<Interaction>, With<ConfirmYesButton>)>,
    mut next: ResMut<NextState<AppState>>,
    mut has_paused: ResMut<crate::state::HasPausedSession>,
    mut showing: ResMut<ShowingQuitConfirm>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            has_paused.0 = false;
            showing.0 = false;
            next.set(AppState::MainMenu);
        }
    }
}

pub fn handle_confirm_cancel(
    q: Query<&Interaction, (Changed<Interaction>, With<ConfirmCancelButton>)>,
    mut showing: ResMut<ShowingQuitConfirm>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            showing.0 = false;
        }
    }
}

/// Spawn the modal when `ShowingQuitConfirm` flips true; despawn when
/// it flips back to false. Driven by `Changed<ShowingQuitConfirm>` so
/// it runs only on the transition, not every frame.
pub fn sync_confirm_modal(
    mut commands: Commands,
    showing: Res<ShowingQuitConfirm>,
    existing: Query<Entity, With<ConfirmModalRoot>>,
) {
    if !showing.is_changed() {
        return;
    }
    for e in existing.iter() {
        commands.entity(e).despawn();
    }
    if !showing.0 {
        return;
    }
    let root = commands
        .spawn((
            ConfirmModalRoot,
            DespawnOnExit(AppState::Paused),
            Node {
                width: percent(100),
                height: percent(100),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: px(24),
                ..default()
            },
            BackgroundColor(COLOR_OVERLAY),
        ))
        .id();
    commands.entity(root).with_children(|parent| {
        parent.spawn((
            Text::new("Quit? Your session will end."),
            TextFont {
                font_size: FONT_HEADER,
                ..default()
            },
            TextColor(COLOR_TEXT),
            Node {
                margin: UiRect::bottom(px(24)),
                ..default()
            },
        ));
        parent
            .spawn((Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(16),
                ..default()
            },))
            .with_children(|row| {
                spawn_button(row, "Yes, quit", ConfirmYesButton, false);
                spawn_button(row, "Cancel", ConfirmCancelButton, false);
            });
    });
}

/// Reset the confirm-modal flag on every entry/exit of Paused so a
/// previous half-cancelled state can't leak into the next pause.
pub fn reset_confirm_flag(mut showing: ResMut<ShowingQuitConfirm>) {
    showing.0 = false;
}
