//! HUD scene: the render-facing text-row contract.
//!
//! A [`Resource`] deliberately, even though today it carries one row.
//! The HUD is a placeholder slated to grow (more rows / readouts); the
//! scene seam exists now so the growth doesn't retrofit it. Music-blind —
//! already-projected display strings, never frequencies or raga names.

use bevy::prelude::*;

/// The render-facing HUD contract: the display rows the widget paints.
/// Glue refreshes it from the [model](super::model); the
/// [systems](super::systems) sync the text nodes from it.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct HudSceneRes {
    /// The math-view row: the scale's tooth-widths, e.g. `int 2 2 1 2 2 2 1`,
    /// or the honest `int —` placeholder when no session is configured.
    pub deg_row: String,
}
