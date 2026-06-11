//! Scale-picker systems: overlay/root/rows/close-button markers and ECS
//! tree spawning + row sync. Reads the [scene](super::scene), knows the
//! engine, not the domain.

use bevy::prelude::*;

use crate::ui::*;

use super::scene::PickerRows;

/// Marker for the picker overlay root so it can be despawned cleanly.
#[derive(Component)]
pub struct ScalePickerRoot;

/// Marker for the row list container so the rows can be located to
/// repopulate when the catalogue arrives.
#[derive(Component)]
pub struct ScalePickerRows;

/// Marker on each clickable scale-shape row. Stores the shape's index into
/// the catalogue so the click handler can look it up without storing a
/// [`ScaleIntervals`](domain_ports::scale::ScaleIntervals) in the component.
#[derive(Component)]
pub struct ScaleRow(pub usize);

/// Marker for the "Close" button.
#[derive(Component)]
pub struct ScalePickerCloseButton;

/// Spawn the overlay tree under `parent` and populate it with `rows` (in
/// catalogue order — row `i` selects catalogue shape `i`). Returns the
/// overlay root entity.
pub fn spawn(commands: &mut Commands, parent: Entity, rows: &PickerRows) -> Entity {
    let root = commands
        .spawn((
            ChildOf(parent),
            ScalePickerRoot,
            Name::new("scale_picker"),
            Node {
                position_type: PositionType::Absolute,
                left: px(32),
                top: px(80),
                width: px(320),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                padding: UiRect::all(px(12)),
                max_height: px(480),
                ..default()
            },
            BackgroundColor(COLOR_OVERLAY),
        ))
        .id();

    // Header label.
    commands.entity(root).with_child((
        Text::new("Scale shape"),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_ACCENT),
        Node {
            margin: UiRect::bottom(px(8)),
            ..default()
        },
    ));

    // Scrollable row list.
    let rows_entity = commands
        .spawn((
            ScalePickerRows,
            Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                overflow: Overflow::scroll_y(),
                ..default()
            },
        ))
        .id();
    commands.entity(root).add_child(rows_entity);
    populate_rows(commands, rows_entity, rows);

    // Close button at the bottom.
    commands
        .entity(root)
        .with_child(picker_row("Close", ScalePickerCloseButton));

    root
}

/// Clear and repopulate the row list entity from `rows` (catalogue order).
pub fn populate_rows(commands: &mut Commands, rows_entity: Entity, rows: &PickerRows) {
    commands.entity(rows_entity).despawn_related::<Children>();
    for (i, row) in rows.iter().enumerate() {
        commands
            .entity(rows_entity)
            .with_child(picker_row(&row.label, ScaleRow(i)));
    }
}

/// A single picker row button with `label` text and the given `marker`.
fn picker_row<M: Component>(label: &str, marker: M) -> impl Bundle {
    (
        Button,
        marker,
        Node {
            width: percent(100),
            height: px(36),
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Center,
            padding: UiRect::horizontal(px(8)),
            ..default()
        },
        BackgroundColor(COLOR_BUTTON),
        children![(
            Text::new(label.to_string()),
            TextFont {
                font_size: FONT_BODY,
                ..default()
            },
            TextColor(COLOR_TEXT),
        )],
    )
}
