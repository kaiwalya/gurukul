//! InGame scale picker: an overlay that opens when the HUD badge is
//! clicked, listing one row per [`ScaleShape`] in [`KnownScales`].
//!
//! # Flow
//!
//! 1. User clicks the HUD badge (`HudBadge` + `Button`).
//! 2. [`handle_hud_click`] sends `Command::ListScales` (populates
//!    [`KnownScales`] via the CQRS round-trip) and sets
//!    [`ShowingScalePicker`] to `true`.
//! 3. [`sync_picker`] detects the flag flip and spawns the overlay,
//!    listing one clickable row per shape with the interval widths as
//!    the label (e.g. `"2 2 1 2 2 2 1"`). A "Close" button closes
//!    without selecting.
//! 4. [`handle_row_click`] reads the selected shape, builds a new
//!    [`Tonality`] from the current tonic + the selected shape's
//!    widths, writes it to [`SongTonality`], sends
//!    `Command::ConfigureSession`, and closes the overlay.
//! 5. The overlay rows also repaint when [`KnownScales`] changes (the
//!    `ListScales` reply lands asynchronously). [`sync_rows`] detects
//!    `Changed<KnownScales>` while the overlay is open and rebuilds
//!    the row children.
//!
//! The session stays live throughout — this is a plain overlay, not a
//! pause. `ConfigureSession` is decoupled from the audio lifecycle.

use crate::coach::Coach;
use crate::state::{AppSettings, AppState, KnownScales, SongTonality};
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::app_coach::Command;
use domain_ports::music::{ScaleShape, Tonality};

/// True while the scale picker overlay is on screen. Flipped by
/// [`handle_hud_click`] (open) and by row / close clicks (close).
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ShowingScalePicker(pub bool);

/// Marker for the picker overlay root so it can be despawned cleanly.
#[derive(Component)]
pub struct ScalePickerRoot;

/// Marker for the row list container so [`sync_rows`] can locate it to
/// repopulate the rows when [`KnownScales`] arrives.
#[derive(Component)]
pub struct ScalePickerRows;

/// Marker on each clickable scale-shape row. Stores the shape index
/// into [`KnownScales`] so the click handler can look it up without
/// storing a [`ScaleShape`] in the component (the component just needs
/// to be small and `Copy`).
#[derive(Component)]
pub struct ScaleRow(pub usize);

/// Marker for the "Close" button.
#[derive(Component)]
pub struct ScalePickerCloseButton;

/// Open the picker: send `ListScales` to populate [`KnownScales`] and
/// show the overlay. Runs only when the HUD badge is pressed and the
/// picker is not already open.
pub fn handle_hud_click(
    q: Query<&Interaction, (Changed<Interaction>, With<crate::game::hud::HudBadge>)>,
    coach: NonSend<Coach>,
    mut showing: ResMut<ShowingScalePicker>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed && !showing.0 {
            coach.0.send_command(Command::ListScales);
            showing.0 = true;
        }
    }
}

/// Spawn or despawn the picker overlay in response to
/// [`ShowingScalePicker`] changing. Rebuilds from scratch on each open
/// so stale rows are never shown.
pub fn sync_picker(
    mut commands: Commands,
    showing: Res<ShowingScalePicker>,
    existing: Query<Entity, With<ScalePickerRoot>>,
    scales: Res<KnownScales>,
) {
    if !showing.is_changed() {
        return;
    }
    // Despawn any existing overlay (covers both close and reopen).
    for e in existing.iter() {
        commands.entity(e).despawn();
    }
    if !showing.0 {
        return;
    }
    spawn_picker(&mut commands, &scales.0);
}

/// Repopulate the row list when [`KnownScales`] changes while the
/// picker is open (the `ListScales` reply arrives asynchronously after
/// the overlay spawns). Runs every frame but no-ops unless both the
/// overlay is visible and the catalogue just changed.
pub fn sync_rows(
    mut commands: Commands,
    showing: Res<ShowingScalePicker>,
    scales: Res<KnownScales>,
    rows_q: Query<Entity, With<ScalePickerRows>>,
) {
    if !showing.0 || !scales.is_changed() {
        return;
    }
    let Ok(rows_entity) = rows_q.single() else {
        return;
    };
    // Clear existing row children and repopulate from the fresh catalogue.
    commands.entity(rows_entity).despawn_related::<Children>();
    for (i, shape) in scales.0.iter().enumerate() {
        let label = shape_label(shape);
        commands
            .entity(rows_entity)
            .with_child(picker_row(&label, ScaleRow(i)));
    }
}

/// On a scale row click: build the new `Tonality` (preserve current
/// tonic, swap the shape), write it to `SongTonality`, send
/// `ConfigureSession`, and close the overlay.
pub fn handle_row_click(
    q: Query<(&Interaction, &ScaleRow), Changed<Interaction>>,
    scales: Res<KnownScales>,
    settings: Res<AppSettings>,
    mut tonality: ResMut<SongTonality>,
    mut showing: ResMut<ShowingScalePicker>,
    coach: NonSend<Coach>,
) {
    for (interaction, ScaleRow(idx)) in q.iter() {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(shape) = scales.0.get(*idx) else {
            continue;
        };
        // Preserve the current tonic; only the shape changes.
        let current_tonic = tonality.0.tonic;
        let widths_f32: Vec<f32> = shape.widths().iter().map(|w| w.0).collect();
        let new_tonality = Tonality::new(current_tonic, &widths_f32);
        tonality.0 = new_tonality;
        coach.0.send_command(Command::ConfigureSession {
            tuning: settings.tuning_spec(),
            tonality: new_tonality,
        });
        showing.0 = false;
        return; // one selection per frame
    }
}

/// Close the picker without selecting a shape.
pub fn handle_close_click(
    q: Query<&Interaction, (Changed<Interaction>, With<ScalePickerCloseButton>)>,
    mut showing: ResMut<ShowingScalePicker>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            showing.0 = false;
        }
    }
}

// ----- helpers -------------------------------------------------------

/// Build the width label for a shape: interval widths joined by spaces,
/// e.g. `"2 2 1 2 2 2 1"`. Each width is rendered as `{:.0}` since
/// widths are whole numbers by invariant.
fn shape_label(shape: &ScaleShape) -> String {
    shape
        .widths()
        .iter()
        .map(|w| format!("{:.0}", w.0))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Spawn the overlay tree. The shape rows are placed in a scrollable
/// child so long catalogues don't overflow the screen.
fn spawn_picker(commands: &mut Commands, shapes: &[ScaleShape]) {
    let root = commands
        .spawn((
            ScalePickerRoot,
            DespawnOnExit(AppState::InGame),
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
    let rows = commands
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
    commands.entity(root).add_child(rows);

    for (i, shape) in shapes.iter().enumerate() {
        let label = shape_label(shape);
        commands
            .entity(rows)
            .with_child(picker_row(&label, ScaleRow(i)));
    }

    // Close button at the bottom.
    commands
        .entity(root)
        .with_child(picker_row("Close", ScalePickerCloseButton));
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
