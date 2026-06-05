//! Note-dial widget — N positions around a circle plus zero or more
//! needles pointing into the circle.
//!
//! The widget is purely geometric. It knows nothing of frequencies,
//! scales, tonics, tuning systems, or labels' meaning. Callers compute
//! angles upstream and hand them over.
//!
//! Convention: `angle = 0` points to 12 o'clock; positive radians
//! rotate clockwise. Callers using standard math convention (0 = 3
//! o'clock, CCW positive) convert before passing in.
//!
//! Two components per dial entity:
//!
//! - [`DialScale`] — geometry (slot angles, labels, active/inactive).
//!   Changes rarely (when the underlying scale/raga/tonic changes).
//! - [`DialState`] — per-frame state (just needles, for now). Changes
//!   frequently. The "current slot" highlight is derived inside the
//!   widget from the primary needle's angle — callers do not specify
//!   it directly.
//!
//! Split so `Changed<DialState>` repaints existing slot children
//! without rebuilding the geometry.

use bevy::prelude::*;
use std::f32::consts::TAU;

/// Geometry of one dial. Spawn alongside a [`DialState`] on the same
/// entity, plus a [`Node`] (or 3D transform) positioning the dial.
#[derive(Component)]
pub struct DialScale {
    pub slots: Vec<DialSlot>,
}

pub struct DialSlot {
    /// Radians, 0 = 12 o'clock, clockwise positive. Should lie in
    /// `[0, TAU)` but callers may pass any value; the widget uses
    /// it directly to position the slot.
    pub angle: f32,
    /// Display label. `None` skips text rendering for that slot.
    pub label: Option<String>,
    /// In the current scale / raga / mode? Inactive slots render with
    /// a dim fill; active ones with a medium fill. Both can still
    /// become "current" if the needle lands on them — the border is
    /// what shows that.
    pub active: bool,
}

/// Per-frame state. Spawn alongside a [`DialScale`] on the same entity.
#[derive(Component, Default)]
pub struct DialState {
    /// Zero or more needles. Each rebuild despawns and respawns all
    /// needle entities — cheap because needles are few and trivial.
    ///
    /// The *first* needle (if any) is the "primary" needle, and the
    /// one used to compute the current-slot highlight. Subsequent
    /// needles are decorative (e.g. a target pitch).
    pub needles: Vec<Needle>,
}

pub struct Needle {
    pub angle: f32,
    pub style: NeedleStyle,
    /// Opacity multiplier in `0.0..=1.0`, applied to the needle
    /// colour's alpha at paint time. Drives "certainty" — a faint
    /// needle is a low-confidence pitch. Defaults to fully opaque for
    /// decorative needles that don't carry a confidence signal.
    pub brightness: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NeedleStyle {
    Primary,
    Secondary,
}

// --- Internals --------------------------------------------------------

/// Child marker for slot dots — owned by the parent dial entity, one
/// per `DialScale::slots` entry, in the same order.
#[derive(Component)]
pub struct SlotDot {
    index: usize,
}

/// Child marker for needles. Despawned and respawned on every state
/// change to keep the count in sync without bookkeeping.
#[derive(Component)]
pub struct NeedleEntity;

// --- Visual constants -------------------------------------------------

const DIAL_RADIUS_PX: f32 = 140.0;
const SLOT_SIZE_PX: f32 = 22.0;
const SLOT_BORDER_PX: f32 = 3.0;
const NEEDLE_LENGTH_PX: f32 = DIAL_RADIUS_PX - SLOT_SIZE_PX;
const NEEDLE_WIDTH_PRIMARY_PX: f32 = 4.0;
const NEEDLE_WIDTH_SECONDARY_PX: f32 = 2.0;
/// Pixel offset from the parent's top-left corner to its centre.
/// The widget assumes the parent is at least `2 * (DIAL_RADIUS_PX +
/// SLOT_SIZE_PX/2)` px wide and tall; callers spawning a smaller
/// container will see slots clipped.
const DIAL_CENTRE_PX: f32 = DIAL_RADIUS_PX + SLOT_SIZE_PX;

/// Inactive slot fill — slot is not in the current scale.
const COLOR_SLOT_INACTIVE: Color = Color::srgb(0.20, 0.20, 0.24);
/// Active slot fill — slot is in the current scale.
const COLOR_SLOT_ACTIVE: Color = Color::srgb(0.45, 0.70, 0.95);
/// Border glow on the current slot when it is in-scale ("right on it").
const COLOR_BORDER_CURRENT_ACTIVE: Color = Color::srgb(1.0, 1.0, 1.0);
/// Border glow on the current slot when it is *out-of-scale*
/// ("you're singing a note that's not in this raga"). Distinct hue so
/// it reads as a warning rather than a target.
const COLOR_BORDER_CURRENT_INACTIVE: Color = Color::srgb(0.95, 0.55, 0.20);
/// Transparent border for slots that aren't current. We always draw a
/// border-width-sized border so the slot's inner size is stable —
/// flipping current on/off only changes the colour, not the geometry.
const COLOR_BORDER_NONE: Color = Color::srgba(0.0, 0.0, 0.0, 0.0);

const COLOR_NEEDLE_PRIMARY: Color = Color::srgb(0.95, 0.95, 0.95);
const COLOR_NEEDLE_SECONDARY: Color = Color::srgba(0.95, 0.95, 0.95, 0.45);

// --- Systems ----------------------------------------------------------

/// Rebuild slot child entities whenever `DialScale` changes. Each slot
/// becomes a small UI node positioned at its angle on a circle of
/// `DIAL_RADIUS_PX`, centred on the parent (via absolute positioning).
pub fn rebuild_slots(
    mut commands: Commands,
    dials: Query<(Entity, &DialScale), Changed<DialScale>>,
    existing_slots: Query<(Entity, &ChildOf), With<SlotDot>>,
) {
    for (dial_entity, scale) in dials.iter() {
        // Despawn old slot children.
        for (child, parent) in existing_slots.iter() {
            if parent.parent() == dial_entity {
                commands.entity(child).despawn();
            }
        }
        // Spawn fresh slot dots.
        for (i, slot) in scale.slots.iter().enumerate() {
            let (x, y) = polar_to_offset(slot.angle, DIAL_RADIUS_PX);
            commands.spawn((
                SlotDot { index: i },
                ChildOf(dial_entity),
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Px(SLOT_SIZE_PX),
                    height: Val::Px(SLOT_SIZE_PX),
                    left: Val::Px(DIAL_CENTRE_PX + x - SLOT_SIZE_PX / 2.0),
                    top: Val::Px(DIAL_CENTRE_PX + y - SLOT_SIZE_PX / 2.0),
                    border: UiRect::all(Val::Px(SLOT_BORDER_PX)),
                    border_radius: BorderRadius::all(Val::Percent(50.0)),
                    ..default()
                },
                BackgroundColor(COLOR_SLOT_INACTIVE),
                BorderColor::all(COLOR_BORDER_NONE),
            ));
        }
    }
}

/// Repaint slot dots and rebuild needles whenever `DialState` *or*
/// `DialScale` changes. Both, because:
/// - State change → recompute the current slot, recolour all slots,
///   respawn needles.
/// - Scale change → `rebuild_slots` (above) just spawned fresh slots in
///   their default-inactive colour; we need a paint pass on the same
///   frame (after the rebuild's commands flush) so the very first
///   render shows the correct active/inactive state.
pub fn apply_state(
    mut commands: Commands,
    dials: Query<(Entity, &DialScale, &DialState), Or<(Changed<DialState>, Changed<DialScale>)>>,
    mut slots: Query<(&SlotDot, &ChildOf, &mut BackgroundColor, &mut BorderColor)>,
    existing_needles: Query<(Entity, &ChildOf), With<NeedleEntity>>,
) {
    for (dial_entity, scale, state) in dials.iter() {
        let current = current_slot(scale, state);

        for (dot, parent, mut bg, mut border) in slots.iter_mut() {
            if parent.parent() != dial_entity {
                continue;
            }
            let Some(slot) = scale.slots.get(dot.index) else {
                continue;
            };
            *bg = BackgroundColor(if slot.active {
                COLOR_SLOT_ACTIVE
            } else {
                COLOR_SLOT_INACTIVE
            });
            *border = BorderColor::all(if current == Some(dot.index) {
                if slot.active {
                    COLOR_BORDER_CURRENT_ACTIVE
                } else {
                    COLOR_BORDER_CURRENT_INACTIVE
                }
            } else {
                COLOR_BORDER_NONE
            });
        }

        for (child, parent) in existing_needles.iter() {
            if parent.parent() == dial_entity {
                commands.entity(child).despawn();
            }
        }
        for needle in &state.needles {
            let (base_color, width) = match needle.style {
                NeedleStyle::Primary => (COLOR_NEEDLE_PRIMARY, NEEDLE_WIDTH_PRIMARY_PX),
                NeedleStyle::Secondary => (COLOR_NEEDLE_SECONDARY, NEEDLE_WIDTH_SECONDARY_PX),
            };
            // Scale the style colour's own alpha by the needle's
            // brightness so confidence ghosts the needle without
            // changing its hue.
            let color = base_color.with_alpha(base_color.alpha() * needle.brightness);
            // To anchor the needle's *base* at the dial centre while
            // pivoting around that base, wrap the needle in a zero-size
            // pivot node positioned at the centre and rotated by `angle`.
            // The needle itself is a child of the pivot, offset so its
            // bottom edge sits at the pivot's origin and it extends
            // upward (12 o'clock at angle = 0). When the pivot rotates,
            // the needle swings around the pivot's origin — the centre.
            let pivot = commands
                .spawn((
                    NeedleEntity,
                    ChildOf(dial_entity),
                    Node {
                        position_type: PositionType::Absolute,
                        width: Val::Px(0.0),
                        height: Val::Px(0.0),
                        left: Val::Px(DIAL_CENTRE_PX),
                        top: Val::Px(DIAL_CENTRE_PX),
                        ..default()
                    },
                    UiTransform::from_rotation(Rot2::radians(needle.angle)),
                ))
                .id();
            commands.spawn((
                ChildOf(pivot),
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Px(width),
                    height: Val::Px(NEEDLE_LENGTH_PX),
                    left: Val::Px(-width / 2.0),
                    top: Val::Px(-NEEDLE_LENGTH_PX),
                    ..default()
                },
                BackgroundColor(color),
            ));
        }
    }
}

/// Find the slot the primary needle is currently "on", or `None` if it
/// sits in the dead band between slots.
///
/// Returns the index of the slot whose centre is within `slot_arc / 6`
/// of the primary needle's angle, where `slot_arc` is the smaller of
/// the two arcs from this slot to its neighbours (handles non-uniform
/// spacings — for 12-TET it simplifies to `TAU/72`, i.e. ±5° around
/// each slot, with a 10° dead zone in the middle of each pair).
///
/// No needles → `None`. The first needle is the primary; later needles
/// are decorative and do not influence the highlight.
fn current_slot(scale: &DialScale, state: &DialState) -> Option<usize> {
    let needle = state.needles.first()?;
    if scale.slots.is_empty() {
        return None;
    }
    let n = scale.slots.len();
    // Pre-compute normalised slot angles.
    let angles: Vec<f32> = scale
        .slots
        .iter()
        .map(|s| s.angle.rem_euclid(TAU))
        .collect();
    let needle_angle = needle.angle.rem_euclid(TAU);

    // Find the nearest slot first.
    let (nearest_idx, nearest_dist) = (0..n)
        .map(|i| (i, angular_distance(angles[i], needle_angle)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;

    // Half the smaller arc from this slot to its (sorted-by-angle)
    // neighbours, divided by 3 for the deadband. Build a sorted index
    // list so neighbours are well-defined even when slots are given out
    // of angular order.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| {
        angles[*a]
            .partial_cmp(&angles[*b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let pos = order.iter().position(|&i| i == nearest_idx)?;
    let prev = order[(pos + n - 1) % n];
    let next = order[(pos + 1) % n];
    let arc_prev = angular_distance(angles[nearest_idx], angles[prev]);
    let arc_next = angular_distance(angles[nearest_idx], angles[next]);
    let smaller_arc = arc_prev.min(arc_next);
    // Highlight band: ±(smaller_arc / 6) → the middle third of the gap
    // to each neighbour is dead.
    let band = smaller_arc / 6.0;

    if nearest_dist <= band {
        Some(nearest_idx)
    } else {
        None
    }
}

/// Shortest angular distance between two angles in `[0, TAU)`.
fn angular_distance(a: f32, b: f32) -> f32 {
    let d = (a - b).abs().rem_euclid(TAU);
    d.min(TAU - d)
}

/// Map an angle (clock convention, 0 = up, clockwise positive) to a
/// pixel offset from the parent's centre.
fn polar_to_offset(angle: f32, radius: f32) -> (f32, f32) {
    // Clock: angle 0 → (0, -radius), TAU/4 → (radius, 0).
    let normalised = angle.rem_euclid(TAU);
    let x = radius * normalised.sin();
    let y = -radius * normalised.cos();
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scale_12tet(active: [bool; 12]) -> DialScale {
        DialScale {
            slots: (0..12)
                .map(|i| DialSlot {
                    angle: i as f32 * TAU / 12.0,
                    label: None,
                    active: active[i],
                })
                .collect(),
        }
    }

    fn with_needle(angle: f32) -> DialState {
        DialState {
            needles: vec![Needle {
                angle,
                style: NeedleStyle::Primary,
                brightness: 1.0,
            }],
        }
    }

    #[test]
    fn current_is_none_when_no_needle() {
        let scale = scale_12tet([true; 12]);
        let state = DialState::default();
        assert_eq!(current_slot(&scale, &state), None);
    }

    #[test]
    fn current_is_slot_when_needle_on_slot() {
        let scale = scale_12tet([true; 12]);
        // Slot 3 is at 3 * TAU/12 = TAU/4.
        let state = with_needle(TAU / 4.0);
        assert_eq!(current_slot(&scale, &state), Some(3));
    }

    #[test]
    fn current_is_none_in_dead_zone_between_slots() {
        // 12 slots → slot arc = TAU/12. Band = TAU/72 each side.
        // Halfway between slot 0 and slot 1 is TAU/24, far outside ±TAU/72.
        let scale = scale_12tet([true; 12]);
        let state = with_needle(TAU / 24.0);
        assert_eq!(current_slot(&scale, &state), None);
    }

    #[test]
    fn current_is_slot_just_inside_band() {
        let scale = scale_12tet([true; 12]);
        // Just inside ±TAU/72 of slot 0.
        let state = with_needle(TAU / 72.0 * 0.99);
        assert_eq!(current_slot(&scale, &state), Some(0));
    }

    #[test]
    fn current_is_none_just_outside_band() {
        let scale = scale_12tet([true; 12]);
        let state = with_needle(TAU / 72.0 * 1.01);
        assert_eq!(current_slot(&scale, &state), None);
    }

    #[test]
    fn inactive_slot_still_eligible_for_current() {
        // Off-scale current — slot 1 (C#) is inactive, but the needle
        // points exactly at it.
        let mut active = [true; 12];
        active[1] = false;
        let scale = scale_12tet(active);
        let state = with_needle(TAU / 12.0);
        assert_eq!(current_slot(&scale, &state), Some(1));
    }
}
