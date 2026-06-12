//! InGame HUD glue: the route surface that stitches the
//! [`hud`](crate::widgets::hud) widget to app state. The panel shows the
//! **math view** of the current tonality, so the singer always has a
//! glanceable, honest answer to "what am I singing against?".
//!
//! Why a math view and not note names: the head is deliberately
//! vocabulary-free (see `docs/MUSIC_MODEL.md`). Until the label layer
//! ships we show the raw numbers the model computes — no invented names to
//! drift out of sync.
//!
//! Source of truth is [`MusicInfoRes`] — the snapshot the coach publishes
//! on `ConfigureSession`. This glue reads it, calls the widget
//! [`model`](crate::widgets::hud::int_row) to project the row, and writes
//! the widget scene ([`HudSceneRes`]); the widget systems paint from there.

use crate::coach::MusicInfoRes;
use crate::game::HudSlot;
use crate::widgets::hud::{self, HudSceneRes};
use bevy::prelude::*;

/// Spawn the HUD widget under the InGame root and force the scene to the
/// current snapshot on entry.
///
/// The forced write matters: the panel's text node spawns empty, so
/// re-entering InGame with *unchanged* music info must still repaint.
/// Relying on `MusicInfoRes` change-detection alone would read re-entry as
/// "nothing changed" and leave the HUD blank. Writing the scene
/// unconditionally here re-arms the widget's `Changed<HudSceneRes>` sync.
pub fn spawn(
    mut commands: Commands,
    music: Res<MusicInfoRes>,
    root: Single<Entity, With<HudSlot>>,
) {
    hud::spawn(&mut commands, *root);
    commands.insert_resource(scene_from(music.0));
}

/// Refresh the HUD scene from [`MusicInfoRes`] when the snapshot changes.
/// The coach handle is not touched here — `drain_events` republishes
/// `music_info()` into the resource.
pub fn refresh(music: Res<MusicInfoRes>, mut scene: ResMut<HudSceneRes>) {
    if !music.is_changed() {
        return;
    }
    let next = scene_from(music.0);
    if *scene != next {
        *scene = next;
    }
}

/// Project a snapshot into the HUD scene. No session configured yet → the
/// honest `int —` placeholder rather than a faked default.
fn scene_from(info: Option<domain_ports::app_coach::MusicInfo>) -> HudSceneRes {
    HudSceneRes {
        deg_row: match info {
            Some(info) => hud::int_row(&info),
            None => "int —".into(),
        },
    }
}
