//! Audio tab of the Settings screen: an input-device picker.
//!
//! Phase 1.6.2a: when mic is not `Granted`, the device list area is
//! replaced by an inline permission panel.

use super::{spawn_choice_row, SettingsTab, TabContent};
use crate::menu::permission::{spawn_permission_panel, MicStatus};
use crate::state::{KnownDevices, SelectedDevice};
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::audio_devices::DeviceId;
use domain_ports::audio_driver::AudioInitStatus;

#[derive(Component)]
pub struct DeviceList;

#[derive(Component, Clone)]
pub struct DeviceRow(pub Option<DeviceId>);

pub(super) fn spawn_tab(commands: &mut Commands, parent: Entity) {
    let container = commands
        .spawn((
            ChildOf(parent),
            TabContent(SettingsTab::Audio),
            Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(12),
                overflow: Overflow::scroll_y(),
                flex_grow: 1.0,
                min_height: px(0),
                ..default()
            },
        ))
        .id();
    commands.spawn((
        ChildOf(container),
        Text::new("Input device"),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT_DIM),
    ));
    commands.spawn((
        ChildOf(container),
        DeviceList,
        Node {
            width: px(520),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        },
    ));
}

/// Rebuild the device-row children whenever `KnownDevices`, `SelectedDevice`,
/// or `MicStatus` changes.
pub fn rebuild_device_list(
    mut commands: Commands,
    known: Res<KnownDevices>,
    selected: Res<SelectedDevice>,
    mic: Res<MicStatus>,
    list: Query<Entity, With<DeviceList>>,
) {
    if !known.is_changed() && !selected.is_changed() && !mic.is_changed() {
        return;
    }
    let Ok(list_entity) = list.single() else {
        return;
    };
    commands.entity(list_entity).despawn_related::<Children>();

    match mic.0 {
        AudioInitStatus::Granted => {
            if known.0.is_empty() {
                commands.spawn((
                    ChildOf(list_entity),
                    Text::new("No microphone found."),
                    TextFont {
                        font_size: FONT_BODY,
                        ..default()
                    },
                    TextColor(COLOR_TEXT_DIM),
                ));
            } else {
                spawn_choice_row(
                    &mut commands,
                    list_entity,
                    DeviceRow(None),
                    "System default",
                    selected.0.is_none(),
                );
                for device in &known.0 {
                    let id = device.persistent_id.clone();
                    let selected_now = selected.0 == id;
                    spawn_choice_row(
                        &mut commands,
                        list_entity,
                        DeviceRow(id),
                        &device.name,
                        selected_now,
                    );
                }
            }
        }
        status => {
            spawn_permission_panel(&mut commands, list_entity, status);
        }
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
