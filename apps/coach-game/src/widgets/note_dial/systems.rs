//! Note-dial systems: Bevy node spawning, markers, painting,
//! current-slot geometry. Reads the [scene](super::scene), knows the
//! engine, not the domain.
//!
//! The widget owns its spawn ([`spawn`]) and stays **placement-agnostic**:
//! it builds the shell, hub, and hub label in neutral/relative layout and
//! takes no placement argument, so the same tree is reusable under any
//! route or test harness. Route placement is applied by glue on the
//! returned shell entity.

use bevy::prelude::*;
use std::f32::consts::TAU;

use crate::ui::*;

use super::scene::{DialScale, DialState, HubState, NeedleStyle};

/// Marker for the dial shell entity so its `DialState` can be looked up
/// each frame without ambiguity. Carries no route vocabulary — placement
/// (InGame overlay, etc.) is a glue concern applied to the same entity.
#[derive(Component)]
pub struct NoteDialRoot;

/// Marker for the dial's **center hub** — a click target over the dial's
/// middle (where Sa lives). Clicking captures the live pitch as the new
/// root; hovering reveals the affordance.
#[derive(Component)]
pub struct DialHub;

/// Marker for the hub's label text, so glue can swap its colour between
/// resting and hover states.
#[derive(Component)]
pub struct DialHubLabel;

/// Child marker for slot dots — owned by the parent dial entity, one per
/// `DialScale::slots` entry, in the same order.
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
const DIAL_CENTRE_PX: f32 = DIAL_RADIUS_PX + SLOT_SIZE_PX;
/// The shell's intrinsic side length: wide enough to hold the ring plus a
/// slot on each side.
const DIAL_BOX_PX: f32 = 324.0;
/// The hub's diameter.
const HUB_SIZE_PX: f32 = 64.0;

/// Inactive slot fill — slot is not in the current scale.
const COLOR_SLOT_INACTIVE: Color = Color::srgb(0.20, 0.20, 0.24);
/// Active slot fill — slot is in the current scale.
const COLOR_SLOT_ACTIVE: Color = Color::srgb(0.45, 0.70, 0.95);
/// Border glow on the current slot when it is in-scale ("right on it").
const COLOR_BORDER_CURRENT_ACTIVE: Color = Color::srgb(1.0, 1.0, 1.0);
/// Border glow on the current slot when it is *out-of-scale*. Distinct hue
/// so it reads as a warning rather than a target.
const COLOR_BORDER_CURRENT_INACTIVE: Color = Color::srgb(0.95, 0.55, 0.20);
/// Transparent border for slots that aren't current. We always draw a
/// border-width-sized border so the slot's inner size is stable.
const COLOR_BORDER_NONE: Color = Color::srgba(0.0, 0.0, 0.0, 0.0);

const COLOR_NEEDLE_PRIMARY: Color = Color::srgb(0.95, 0.95, 0.95);
const COLOR_NEEDLE_SECONDARY: Color = Color::srgba(0.95, 0.95, 0.95, 0.45);

// --- Spawn ------------------------------------------------------------

/// Spawn the dial shell, hub, and hub label under `parent`, **empty** (no
/// slots yet) and in neutral/relative layout. Returns the shell entity so
/// glue can apply route placement to its `Node`.
///
/// The shell carries [`DialScale`] / [`DialState`] (the scene contract glue
/// writes), a [`Button`] so the whole box is pickable for hub hover, and
/// [`ButtonSelected`] to opt out of the generic repaint.
pub fn spawn(commands: &mut Commands, parent: Entity) -> Entity {
    let dial = commands
        .spawn((
            ChildOf(parent),
            NoteDialRoot,
            Button,
            ButtonSelected,
            Node {
                width: px(DIAL_BOX_PX),
                height: px(DIAL_BOX_PX),
                // Center the hub child over the dial's middle (Sa).
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            DialScale { slots: Vec::new() },
            DialState::default(),
        ))
        .id();

    // Center hub: the click target. Spawns transparent (invisible until
    // hovered); glue paints the three-state look. `ButtonSelected` opts it
    // out of the generic hover/press repaint.
    commands.entity(dial).with_child((
        Button,
        DialHub,
        ButtonSelected,
        Node {
            width: px(HUB_SIZE_PX),
            height: px(HUB_SIZE_PX),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(percent(50)),
            ..default()
        },
        BackgroundColor(Color::NONE),
        children![(
            DialHubLabel,
            Text::new("Zero"),
            TextFont {
                font_size: FONT_BODY,
                ..default()
            },
            TextColor(Color::NONE),
        )],
    ));

    dial
}

/// Map a resolved [`HubState`] to the hub's (background, text) colour pair.
/// The enum → colour mapping lives here (the Bevy layer); the model resolves
/// the enum without touching `bevy::Color`.
pub fn hub_colors(state: HubState) -> (Color, Color) {
    match state {
        HubState::Hidden => (Color::NONE, Color::NONE),
        HubState::Disabled => (COLOR_BUTTON_DISABLED, COLOR_TEXT_DIM),
        HubState::Enabled => (COLOR_BUTTON, COLOR_TEXT),
        HubState::Pressed => (COLOR_BUTTON_PRESSED, COLOR_TEXT),
    }
}

// --- Painting ---------------------------------------------------------

/// Rebuild slot child entities whenever `DialScale` changes. Each slot
/// becomes a small UI node positioned at its angle on a circle of
/// `DIAL_RADIUS_PX`, centred on the parent (via absolute positioning).
pub fn rebuild_slots(
    mut commands: Commands,
    dials: Query<(Entity, &DialScale), Changed<DialScale>>,
    existing_slots: Query<(Entity, &ChildOf), With<SlotDot>>,
) {
    for (dial_entity, scale) in dials.iter() {
        for (child, parent) in existing_slots.iter() {
            if parent.parent() == dial_entity {
                commands.entity(child).despawn();
            }
        }
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
/// `DialScale` changes (the scale path repaints the fresh slots
/// `rebuild_slots` just spawned, on the same frame).
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
            let color = base_color.with_alpha(base_color.alpha() * needle.brightness);
            // Wrap the needle in a zero-size pivot node at the centre,
            // rotated by `angle`; the needle child extends upward so its
            // base sits at the pivot's origin (12 o'clock at angle = 0).
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

// --- Current-slot geometry --------------------------------------------

/// Find the slot the primary needle is currently "on", or `None` if it
/// sits in the dead band between slots.
///
/// Returns the index of the slot whose centre is within `slot_arc / 6` of
/// the primary needle's angle, where `slot_arc` is the smaller of the two
/// arcs from this slot to its neighbours.
///
/// No needles → `None`. The first needle is the primary; later needles are
/// decorative and do not influence the highlight.
fn current_slot(scale: &DialScale, state: &DialState) -> Option<usize> {
    let needle = state.needles.first()?;
    if scale.slots.is_empty() {
        return None;
    }
    let n = scale.slots.len();
    let angles: Vec<f32> = scale
        .slots
        .iter()
        .map(|s| s.angle.rem_euclid(TAU))
        .collect();
    let needle_angle = needle.angle.rem_euclid(TAU);

    let (nearest_idx, nearest_dist) = (0..n)
        .map(|i| (i, angular_distance(angles[i], needle_angle)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;

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

/// Map an angle (clock convention, 0 = up, clockwise positive) to a pixel
/// offset from the parent's centre.
fn polar_to_offset(angle: f32, radius: f32) -> (f32, f32) {
    let normalised = angle.rem_euclid(TAU);
    let x = radius * normalised.sin();
    let y = -radius * normalised.cos();
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::super::scene::{DialSlot, Needle};
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
        let state = with_needle(TAU / 4.0);
        assert_eq!(current_slot(&scale, &state), Some(3));
    }

    #[test]
    fn current_is_none_in_dead_zone_between_slots() {
        let scale = scale_12tet([true; 12]);
        let state = with_needle(TAU / 24.0);
        assert_eq!(current_slot(&scale, &state), None);
    }

    #[test]
    fn current_is_slot_just_inside_band() {
        let scale = scale_12tet([true; 12]);
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
        let mut active = [true; 12];
        active[1] = false;
        let scale = scale_12tet(active);
        let state = with_needle(TAU / 12.0);
        assert_eq!(current_slot(&scale, &state), Some(1));
    }
}
