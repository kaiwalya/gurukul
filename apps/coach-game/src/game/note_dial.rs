//! InGame note-dial glue: the route surface that stitches the
//! [`note_dial`](crate::widgets::note_dial) widget to app state. North (12
//! o'clock) is always **Sa (the song's tonic)**; the dial tracks the
//! coach's live `f0` as the primary needle.
//!
//! This file is the only place the two worlds touch: it reads
//! [`MusicInfoRes`] / [`LatestFeatures`], calls the widget `model` to turn
//! those domain facts into geometry, writes the widget scene
//! (`DialScale` / `DialState`), and sends `ConfigureSession` on capture-Sa.
//! No frequency math lives here — the model spent the music.

use crate::coach::{Coach, LatestFeatures, MusicInfoRes};
use crate::game::InGameRoot;
use crate::state::{AppSettings, SongTonality};
use crate::widgets::note_dial::{
    self, hub_colors, hub_visual_state, is_capture_voiced, DialHub, DialHubLabel, DialScale,
    DialState, NoteDialRoot,
};
use bevy::prelude::*;
use domain_ports::app_coach::Command;

/// Spawn the dial widget under the InGame root and apply the route's
/// bottom-right overlay placement to the returned shell entity.
///
/// The widget builds itself placement-agnostic (neutral layout); InGame is
/// route vocabulary, so the absolute `right`/`bottom` positioning is set
/// here, on the existing `Node` (no `Transform` — UI nodes use `Node`).
/// The shell spawns empty: the tuning + tonality come from the coach's read
/// model ([`MusicInfoRes`]), which may not have landed yet. [`repaint_slots`]
/// fills the slots in as soon as the snapshot is available.
pub fn spawn(mut commands: Commands, root: Single<Entity, With<InGameRoot>>) {
    let dial = note_dial::spawn(&mut commands, *root);
    // Apply InGame overlay placement on the just-spawned shell's Node. The
    // widget set width/height/centering; keep those, add absolute placement.
    commands
        .entity(dial)
        .entry::<Node>()
        .and_modify(|mut node| {
            node.position_type = PositionType::Absolute;
            node.right = px(80);
            node.bottom = px(80);
        });
}

/// Paint the dial's slots from the [`MusicInfoRes`] read model. Writes a
/// fresh [`DialScale`] (which the widget's `rebuild_slots` repaints on)
/// when either the snapshot just changed, or the dial still has no slots
/// (freshly spawned this InGame visit while the resource already held a
/// `Some` from a prior session). No `Some` snapshot yet → leave it empty.
pub fn repaint_slots(
    music: Res<MusicInfoRes>,
    mut dial: Query<&mut DialScale, With<NoteDialRoot>>,
) {
    let Some(info) = music.0 else {
        return;
    };
    let Ok(mut scale) = dial.single_mut() else {
        return;
    };
    if !music.is_changed() && !scale.slots.is_empty() {
        return;
    }
    scale.slots = note_dial::build_slots(&info);
}

/// Each frame, read the latest [`Features`](crate::coach::Features) and
/// update the dial's `DialState.needles` via the widget model. Voiced → one
/// primary needle; unvoiced / no snapshot → empty `needles`.
///
/// We don't dedupe on `t_ms`: leaving `DialState` unchanged is idempotent
/// and skipping the write avoids re-triggering `Changed<DialState>` every
/// frame, which would respawn needles unnecessarily.
pub fn update_from_features(
    features: Res<LatestFeatures>,
    music: Res<MusicInfoRes>,
    mut dial: Query<&mut DialState, With<NoteDialRoot>>,
) {
    let Ok(mut state) = dial.single_mut() else {
        return;
    };
    let (Some(snap), Some(info)) = (features.0, music.0) else {
        // No snapshot or no music config → ensure no needle.
        if !state.needles.is_empty() {
            state.needles.clear();
        }
        return;
    };

    match note_dial::project_needle(&info, snap.pitch, snap.confidence) {
        Some(needle) => {
            // Replace any prior needle. Writing through DerefMut triggers
            // change detection on DialState, which the widget repaints on.
            state.needles.clear();
            state.needles.push(needle);
        }
        None => {
            if !state.needles.is_empty() {
                state.needles.clear();
            }
        }
    }
}

/// Click the center hub → capture the live pitch as the song's tonic (Sa).
///
/// Resolves the live `f0` to the nearest tuning groove (register preserved)
/// and rebuilds the [`Scale`](domain_ports::scale::Scale) via the widget
/// model ([`note_dial::capture_scale`]), keeping the tooth pattern but
/// re-rooting Sa. Writes [`SongTonality`] and round-trips it through the
/// coach. Gated on confidence — a no-op below the model's capture gate (the
/// hub is also greyed by [`sync_hub`]; this is the guard).
pub fn handle_hub_capture(
    q: Query<&Interaction, (Changed<Interaction>, With<DialHub>)>,
    features: Res<LatestFeatures>,
    settings: Res<AppSettings>,
    mut tonality: ResMut<SongTonality>,
    coach: NonSend<Coach>,
) {
    for i in q.iter() {
        if *i != Interaction::Pressed {
            continue;
        }
        let Some(snap) = features.0 else {
            return;
        };
        let Some(pitch) = snap.pitch else {
            return; // gate: unvoiced
        };
        if !is_capture_voiced(snap.pitch, snap.confidence) {
            return; // gate: weakly-voiced
        }
        let absolute = settings.tuning_absolute();
        let new_scale = note_dial::capture_scale(&absolute, &tonality.0, pitch);
        info!(
            "capture-Sa: f0={:.1}Hz conf={:.2}",
            pitch.to_hz(),
            snap.confidence,
        );
        tonality.0 = new_scale;
        coach
            .0
            .send_command(Command::ConfigureSession { scale: new_scale });
        return;
    }
}

/// Paint the center hub from **dial hover** + live confidence. Resolves the
/// three-state look in the widget model ([`hub_visual_state`]) and maps it
/// to colours via the widget systems ([`hub_colors`]) — the glue only wires
/// the queries together.
///
/// "Hovering the dial" means the pointer is over the dial box *or* the hub
/// itself — the hub child would otherwise occlude the dial's own hover and
/// make itself vanish exactly as you reach to click it.
pub fn sync_hub(
    dial_q: Query<&Interaction, With<NoteDialRoot>>,
    hub_q: Query<&Interaction, With<DialHub>>,
    features: Res<LatestFeatures>,
    mut bg_q: Query<&mut BackgroundColor, With<DialHub>>,
    mut label_q: Query<&mut TextColor, With<DialHubLabel>>,
) {
    let dial_hovered = dial_q
        .single()
        .map(|i| *i != Interaction::None)
        .unwrap_or(false);
    let hub_interaction = hub_q.single().copied().unwrap_or(Interaction::None);
    let hovered = dial_hovered || hub_interaction != Interaction::None;
    let voiced = features
        .0
        .map(|s| is_capture_voiced(s.pitch, s.confidence))
        .unwrap_or(false);

    let state = hub_visual_state(hovered, hub_interaction == Interaction::Pressed, voiced);
    let (bg, text) = hub_colors(state);

    // Only write on change so we don't retrigger change-detection each frame.
    if let Ok(mut color) = bg_q.single_mut() {
        if color.0 != bg {
            color.0 = bg;
        }
    }
    if let Ok(mut color) = label_q.single_mut() {
        if color.0 != text {
            color.0 = text;
        }
    }
}
