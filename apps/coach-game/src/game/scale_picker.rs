//! InGame scale-picker glue: the route surface that stitches the
//! [`scale_picker`](crate::widgets::scale_picker) widget to app state and
//! coach commands.
//!
//! # Flow
//!
//! 1. User clicks the HUD badge ([`HudBadge`](crate::widgets::hud::HudBadge)).
//! 2. [`handle_hud_click`] sends `Command::ListScales` (populates
//!    [`KnownScales`] via the CQRS round-trip) and sets
//!    [`ShowingScalePicker`] to `true`.
//! 3. [`sync_picker`] detects the flag flip and spawns the widget overlay.
//! 4. [`sync_rows`] repopulates the rows when [`KnownScales`] arrives (the
//!    `ListScales` reply lands asynchronously) while the overlay is open.
//! 5. [`handle_row_click`] computes the selected [`Scale`] via the widget
//!    model, writes it to [`SongTonality`], sends `ConfigureSession`, and
//!    closes the overlay.
//!
//! The session stays live throughout — this is a plain overlay, not a
//! pause. `ConfigureSession` is decoupled from the audio lifecycle.
//!
//! Open/closed visibility ([`ShowingScalePicker`]) is a route/interaction
//! concern and stays in glue, not the widget scene: the scene describes the
//! rows ("given it's open, here is what to render"); glue owns "should it be
//! open."

use crate::coach::Coach;
use crate::game::InGameRoot;
use crate::state::{KnownScales, SongTonality};
use crate::widgets::hud::HudBadge;
use crate::widgets::scale_picker::{
    self, PickerRow, PickerRows, ScalePickerCloseButton, ScalePickerRows, ScaleRow,
};
use bevy::prelude::*;
use domain_ports::app_coach::Command;
use domain_ports::scale::ScaleIntervals;
use domain_ports::tuning::Tuning;

/// True while the scale picker overlay is on screen. Flipped by
/// [`handle_hud_click`] (open) and by row / close clicks (close).
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ShowingScalePicker(pub bool);

/// Open the picker: send `ListScales` to populate [`KnownScales`] and show
/// the overlay. Runs only when the HUD badge is pressed and the picker is
/// not already open.
pub fn handle_hud_click(
    q: Query<&Interaction, (Changed<Interaction>, With<HudBadge>)>,
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
/// [`ShowingScalePicker`] changing. Rebuilds from scratch on each open so
/// stale rows are never shown.
pub fn sync_picker(
    mut commands: Commands,
    showing: Res<ShowingScalePicker>,
    existing: Query<Entity, With<scale_picker::ScalePickerRoot>>,
    scales: Res<KnownScales>,
    tonality: Res<SongTonality>,
    root: Single<Entity, With<InGameRoot>>,
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
    let rows = picker_rows(&scales.0, tonality.0.tuning().len() as u32);
    scale_picker::spawn(&mut commands, *root, &rows);
}

/// Repopulate the row list when [`KnownScales`] changes while the picker is
/// open (the `ListScales` reply arrives asynchronously after the overlay
/// spawns). No-ops unless both the overlay is visible and the catalogue
/// just changed.
pub fn sync_rows(
    mut commands: Commands,
    showing: Res<ShowingScalePicker>,
    scales: Res<KnownScales>,
    tonality: Res<SongTonality>,
    rows_q: Query<Entity, With<ScalePickerRows>>,
) {
    if !showing.0 || !scales.is_changed() {
        return;
    }
    let Ok(rows_entity) = rows_q.single() else {
        return;
    };
    let rows = picker_rows(&scales.0, tonality.0.tuning().len() as u32);
    scale_picker::populate_rows(&mut commands, rows_entity, &rows);
}

/// On a scale row click: compute the new [`Scale`] via the widget model
/// (keeping the current Sa rotation + register, swapping the tooth
/// pattern), write it to [`SongTonality`], send `ConfigureSession`, and
/// close the overlay.
pub fn handle_row_click(
    q: Query<(&Interaction, &ScaleRow), Changed<Interaction>>,
    scales: Res<KnownScales>,
    mut tonality: ResMut<SongTonality>,
    mut showing: ResMut<ShowingScalePicker>,
    coach: NonSend<Coach>,
) {
    for (interaction, ScaleRow(idx)) in q.iter() {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(&intervals) = scales.0.get(*idx) else {
            continue;
        };
        let new_scale = scale_picker::select_scale(&tonality.0, intervals);
        tonality.0 = new_scale;
        coach
            .0
            .send_command(Command::ConfigureSession { scale: new_scale });
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

/// Project the catalogue into the widget's row scene (catalogue order;
/// row `i` selects `scales[i]`).
fn picker_rows(scales: &[ScaleIntervals], n: u32) -> PickerRows {
    scale_picker::row_labels(scales, n)
        .into_iter()
        .map(|label| PickerRow { label })
        .collect()
}
