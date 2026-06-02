//! Audio tab of the Settings screen: an input-device picker.
//!
//! The tab container is spawned at `OnEnter(Settings)` via
//! [`spawn_tab`]. The device list is repopulated each time
//! [`KnownDevices`] or [`SelectedDevice`] changes. Clicking a row
//! writes to [`SelectedDevice`]; the selected device is consumed by
//! `coach::start_session_on_enter_in_game` when a session begins.

use super::{spawn_choice_row, SettingsTab, TabContent};
use crate::state::{KnownDevices, SelectedDevice};
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::audio_devices::DeviceId;

/// Marker for the scrollable area where device rows live. We despawn
/// + respawn its children whenever `KnownDevices` changes.
#[derive(Component)]
pub struct DeviceList;

/// Attached to each device row so the click handler knows which
/// device id (or `None` for default) to write into `SelectedDevice`.
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
                // Fill the vertical space between tab bar and Back button.
                // `min_height: 0` is the flex escape hatch that lets the
                // container shrink below its content's intrinsic height,
                // so `overflow: scroll_y` has something to clip against.
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
