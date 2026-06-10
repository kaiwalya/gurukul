//! Note-dial scene: the render-facing contract.
//!
//! Already-projected geometry only — slot angles, needle angles, slot
//! activity, and the resolved hub visual state. Music-blind: no
//! frequencies, scales, or tonics. Glue and widget systems meet on the
//! dial entity through [`DialScale`] / [`DialState`] (components), and
//! the hub through [`HubState`] (a plain enum the systems map to colour).
//!
//! Convention: `angle = 0` points to 12 o'clock; positive radians rotate
//! clockwise.

use bevy::prelude::*;

/// Geometry of one dial. Spawn alongside a [`DialState`] on the same
/// entity, plus a [`Node`] positioning the dial.
#[derive(Component)]
pub struct DialScale {
    pub slots: Vec<DialSlot>,
}

pub struct DialSlot {
    /// Radians, 0 = 12 o'clock, clockwise positive. Should lie in
    /// `[0, TAU)` but callers may pass any value; the widget uses it
    /// directly to position the slot.
    pub angle: f32,
    /// Display label. `None` skips text rendering for that slot.
    pub label: Option<String>,
    /// In the current scale / raga / mode? Inactive slots render with a
    /// dim fill; active ones with a medium fill. Both can still become
    /// "current" if the needle lands on them — the border is what shows
    /// that.
    pub active: bool,
}

/// Per-frame state. Spawn alongside a [`DialScale`] on the same entity.
#[derive(Component, Default)]
pub struct DialState {
    /// Zero or more needles. Each rebuild despawns and respawns all
    /// needle entities — cheap because needles are few and trivial.
    ///
    /// The *first* needle (if any) is the "primary" needle, and the one
    /// used to compute the current-slot highlight. Subsequent needles
    /// are decorative (e.g. a target pitch).
    pub needles: Vec<Needle>,
}

pub struct Needle {
    pub angle: f32,
    pub style: NeedleStyle,
    /// Opacity multiplier in `0.0..=1.0`, applied to the needle colour's
    /// alpha at paint time. Drives "certainty" — a faint needle is a
    /// low-confidence pitch. Defaults to fully opaque for decorative
    /// needles that don't carry a confidence signal.
    pub brightness: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NeedleStyle {
    Primary,
    Secondary,
}

/// Resolved three-state look of the dial's centre hub, the read model
/// the [model](super::model::hub_visual_state) produces from dial hover +
/// live confidence. The [systems](super::systems) layer maps each variant
/// to a (background, text) colour pair — the model stays Bevy-free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HubState {
    /// Not hovering the dial → invisible (but still clickable).
    Hidden,
    /// Hovering, below the confidence gate → visible but greyed.
    Disabled,
    /// Hovering, above the gate → visible + enabled.
    Enabled,
    /// Hovering, above the gate, pressed → press feedback.
    Pressed,
}
