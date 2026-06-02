//! Settings screen: Audio tab with a device picker + Back.
//!
//! On enter: request device enumeration via `Command::ListDevices`. The
//! response lands in `KnownDevices` (populated by `coach::drain_events`)
//! and a second system rebuilds the list when that resource changes.
//!
//! Selection: click a row → write `SelectedDevice`. Default ("system
//! default") is always at the top of the list.

use crate::coach::Coach;
use crate::state::{AppState, KnownDevices, SelectedDevice};
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::app_coach::Command;
use domain_ports::audio_devices::DeviceId;

#[derive(Component)]
pub struct BackButton;

/// Marker for the scrollable area where device rows live. We despawn
/// + respawn its children whenever `KnownDevices` changes.
#[derive(Component)]
pub struct DeviceList;

/// Attached to each device row so the click handler knows which
/// device id (or `None` for default) to write into `SelectedDevice`.
#[derive(Component, Clone)]
pub struct DeviceRow(pub Option<DeviceId>);

pub fn on_enter(coach: NonSend<Coach>, mut commands: Commands) {
    // Kick off a fresh enumeration. `KnownDevices` will be populated
    // within a frame or two by `coach::drain_events`.
    coach.0.send_command(Command::ListDevices);

    commands.spawn((
        DespawnOnExit(AppState::Settings),
        Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            padding: UiRect::all(px(32)),
            row_gap: px(20),
            ..default()
        },
        BackgroundColor(COLOR_BG),
        children![
            (
                Text::new("Settings"),
                TextFont {
                    font_size: FONT_TITLE,
                    ..default()
                },
                TextColor(COLOR_ACCENT),
            ),
            (
                Text::new("Audio"),
                TextFont {
                    font_size: FONT_HEADER,
                    ..default()
                },
                TextColor(COLOR_TEXT),
                Node {
                    margin: UiRect::top(px(8)),
                    ..default()
                },
            ),
            (
                Text::new("Input device"),
                TextFont {
                    font_size: FONT_BODY,
                    ..default()
                },
                TextColor(COLOR_TEXT_DIM),
            ),
            // The device list itself. Children populated by
            // `rebuild_device_list` when `KnownDevices` changes.
            (
                DeviceList,
                Node {
                    width: px(520),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(6),
                    ..default()
                },
            ),
            (
                Button,
                BackButton,
                Node {
                    width: px(160),
                    height: px(48),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    margin: UiRect::top(px(32)),
                    ..default()
                },
                BackgroundColor(COLOR_BUTTON),
                children![(
                    Text::new("Back"),
                    TextFont {
                        font_size: FONT_BUTTON,
                        ..default()
                    },
                    TextColor(COLOR_TEXT),
                )],
            ),
        ],
    ));
}

/// Rebuild the device-row children whenever `KnownDevices` changes
/// (e.g. when `ListDevices` first returns) or when `SelectedDevice`
/// changes (to update the row highlight).
pub fn rebuild_device_list(
    mut commands: Commands,
    known: Res<KnownDevices>,
    selected: Res<SelectedDevice>,
    list: Query<Entity, With<DeviceList>>,
) {
    if !known.is_changed() && !selected.is_changed() {
        return;
    }
    let Ok(list_entity) = list.single() else {
        return;
    };
    commands.entity(list_entity).despawn_related::<Children>();

    spawn_device_row(
        &mut commands,
        list_entity,
        None,
        "System default",
        selected.0.is_none(),
    );
    for device in &known.0 {
        let id = device.persistent_id.clone();
        let selected_now = selected.0 == id;
        spawn_device_row(&mut commands, list_entity, id, &device.name, selected_now);
    }
}

fn spawn_device_row(
    commands: &mut Commands,
    list_entity: Entity,
    id: Option<DeviceId>,
    label: &str,
    selected_now: bool,
) {
    let row = commands
        .spawn((
            Button,
            DeviceRow(id),
            Node {
                padding: UiRect::axes(px(12), px(8)),
                width: percent(100),
                ..default()
            },
            BackgroundColor(row_color(selected_now)),
            ChildOf(list_entity),
        ))
        .id();
    if selected_now {
        commands.entity(row).insert(ButtonSelected);
    }
    commands.entity(row).with_child((
        Text::new(label.to_string()),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT),
    ));
}

fn row_color(selected: bool) -> Color {
    if selected {
        COLOR_ACCENT
    } else {
        COLOR_BUTTON
    }
}

pub fn handle_row_click(
    q: Query<(&Interaction, &DeviceRow), (Changed<Interaction>, With<Button>)>,
    mut selected: ResMut<SelectedDevice>,
) {
    for (interaction, row) in q.iter() {
        if *interaction == Interaction::Pressed {
            selected.0 = row.0.clone();
        }
    }
}

pub fn handle_back(
    q: Query<&Interaction, (Changed<Interaction>, With<BackButton>)>,
    mut next: ResMut<NextState<AppState>>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            next.set(AppState::MainMenu);
        }
    }
}
