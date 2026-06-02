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
//! - [`DialScale`] — geometry (slot angles + optional labels). Changes
//!   rarely (when the underlying scale/raga/tonic changes).
//! - [`DialState`] — per-frame state (slot lit/off, needle angles).
//!   Changes frequently.
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
}

/// Per-frame state. Spawn alongside a [`DialScale`] on the same entity.
#[derive(Component, Default)]
pub struct DialState {
    /// One entry per [`DialScale::slots`] entry, same length and order.
    /// Length mismatch is a caller bug; the repaint system asserts.
    pub slots: Vec<SlotState>,
    /// Zero or more needles. Each rebuild despawns and respawns all
    /// needle entities — cheap because needles are few and trivial.
    pub needles: Vec<Needle>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SlotState {
    #[default]
    Off,
    Lit,
}

pub struct Needle {
    pub angle: f32,
    pub style: NeedleStyle,
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
const NEEDLE_LENGTH_PX: f32 = DIAL_RADIUS_PX - SLOT_SIZE_PX;
const NEEDLE_WIDTH_PRIMARY_PX: f32 = 4.0;
const NEEDLE_WIDTH_SECONDARY_PX: f32 = 2.0;
/// Pixel offset from the parent's top-left corner to its centre.
/// The widget assumes the parent is at least `2 * (DIAL_RADIUS_PX +
/// SLOT_SIZE_PX/2)` px wide and tall; callers spawning a smaller
/// container will see slots clipped.
const DIAL_CENTRE_PX: f32 = DIAL_RADIUS_PX + SLOT_SIZE_PX;

const COLOR_SLOT_OFF: Color = Color::srgb(0.20, 0.20, 0.24);
const COLOR_SLOT_LIT: Color = Color::srgb(0.45, 0.70, 0.95);
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
                    border_radius: BorderRadius::all(Val::Percent(50.0)),
                    ..default()
                },
                BackgroundColor(COLOR_SLOT_OFF),
            ));
        }
    }
}

/// Repaint slot dots and rebuild needles whenever `DialState` *or*
/// `DialScale` changes. Both, because:
/// - State change → recolour existing slots, respawn needles.
/// - Scale change → `rebuild_slots` (above) just spawned fresh slots in
///   their default-off colour; we need a paint pass on the same frame
///   (after the rebuild's commands flush) so the very first render
///   shows the correct lit/off state.
pub fn apply_state(
    mut commands: Commands,
    dials: Query<(Entity, &DialScale, &DialState), Or<(Changed<DialState>, Changed<DialScale>)>>,
    mut slots: Query<(&SlotDot, &ChildOf, &mut BackgroundColor)>,
    existing_needles: Query<(Entity, &ChildOf), With<NeedleEntity>>,
) {
    for (dial_entity, scale, state) in dials.iter() {
        debug_assert_eq!(
            scale.slots.len(),
            state.slots.len(),
            "DialState.slots length must match DialScale.slots"
        );

        for (dot, parent, mut bg) in slots.iter_mut() {
            if parent.parent() != dial_entity {
                continue;
            }
            let Some(s) = state.slots.get(dot.index) else {
                continue;
            };
            *bg = match s {
                SlotState::Off => BackgroundColor(COLOR_SLOT_OFF),
                SlotState::Lit => BackgroundColor(COLOR_SLOT_LIT),
            };
        }

        for (child, parent) in existing_needles.iter() {
            if parent.parent() == dial_entity {
                commands.entity(child).despawn();
            }
        }
        for needle in &state.needles {
            let (color, width) = match needle.style {
                NeedleStyle::Primary => (COLOR_NEEDLE_PRIMARY, NEEDLE_WIDTH_PRIMARY_PX),
                NeedleStyle::Secondary => (COLOR_NEEDLE_SECONDARY, NEEDLE_WIDTH_SECONDARY_PX),
            };
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

/// Map an angle (clock convention, 0 = up, clockwise positive) to a
/// pixel offset from the parent's centre.
fn polar_to_offset(angle: f32, radius: f32) -> (f32, f32) {
    // Clock: angle 0 → (0, -radius), TAU/4 → (radius, 0).
    let normalised = angle.rem_euclid(TAU);
    let x = radius * normalised.sin();
    let y = -radius * normalised.cos();
    (x, y)
}
