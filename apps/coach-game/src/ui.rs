//! Shared UI styling for the menu shell.
//!
//! No theming system yet — these constants are the whole palette. When
//! the look starts to mature we'll likely move to a `Theme` resource.

use bevy::prelude::*;

pub const COLOR_BG: Color = Color::srgb(0.08, 0.08, 0.10);
/// Semi-opaque background for overlays drawn on top of another state
/// (paused menu, confirm dialogs). Slightly transparent so the previous
/// screen reads as still-present-but-inactive.
pub const COLOR_OVERLAY: Color = Color::srgba(0.04, 0.04, 0.06, 0.92);
pub const COLOR_BUTTON: Color = Color::srgb(0.18, 0.18, 0.22);
pub const COLOR_BUTTON_HOVER: Color = Color::srgb(0.28, 0.28, 0.34);
pub const COLOR_BUTTON_PRESSED: Color = Color::srgb(0.12, 0.12, 0.16);
pub const COLOR_BUTTON_DISABLED: Color = Color::srgb(0.13, 0.13, 0.15);
pub const COLOR_TEXT: Color = Color::srgb(0.92, 0.92, 0.95);
pub const COLOR_TEXT_DIM: Color = Color::srgb(0.55, 0.55, 0.60);
pub const COLOR_ACCENT: Color = Color::srgb(0.45, 0.70, 0.95);

pub const FONT_TITLE: f32 = 56.0;
pub const FONT_HEADER: f32 = 32.0;
pub const FONT_BUTTON: f32 = 26.0;
pub const FONT_BODY: f32 = 20.0;

/// Tag a `Button` with this to opt it out of hover/press repaint and
/// out of click-handler matching. Used for the Settings entry in the
/// Paused overlay (you can't change input devices mid-run).
#[derive(Component)]
pub struct ButtonDisabled;

/// Tag a `Button` with this to opt it out of hover/press repaint —
/// its background stays whatever it was spawned with. Used for the
/// currently-selected row in the device list (drawn in `COLOR_ACCENT`
/// by `rebuild_device_list`), so the generic interaction repaint
/// doesn't wipe the selection highlight on mouse-out.
#[derive(Component)]
pub struct ButtonSelected;

/// Repaint the button's background each frame based on its interaction
/// state. Pair with any `Button` entity; the system below runs in
/// `Update` and covers every button regardless of screen. Skips
/// disabled and selected buttons so they keep their own paint.
pub fn update_button_colors(
    mut q: Query<
        (&Interaction, &mut BackgroundColor),
        (
            Changed<Interaction>,
            With<Button>,
            Without<ButtonDisabled>,
            Without<ButtonSelected>,
        ),
    >,
) {
    for (interaction, mut bg) in q.iter_mut() {
        *bg = match *interaction {
            Interaction::Pressed => BackgroundColor(COLOR_BUTTON_PRESSED),
            Interaction::Hovered => BackgroundColor(COLOR_BUTTON_HOVER),
            Interaction::None => BackgroundColor(COLOR_BUTTON),
        };
    }
}

// --- Mouse-wheel scrolling -------------------------------------------
//
// Bevy 0.18 ships `Overflow::scroll_y()` / `Overflow::scroll_x()` for
// *clipping*, but does not auto-scroll on mouse wheel — you wire the
// `MouseWheel → ScrollPosition` pipeline yourself. The pattern below
// is the canonical one from `examples/ui/scroll_and_overflow/scroll.rs`
// in the Bevy repo.
//
// Flow: read `MouseWheel` events → for each entity in `HoverMap`
// (entities the pointer is currently over), trigger a `Scroll` entity
// event. The event is `auto_propagate`d up the UI hierarchy so a wheel
// over a text child finds the scrollable container ancestor. The
// observer mutates `ScrollPosition` on the first ancestor with a
// matching `OverflowAxis::Scroll`, clamped to the content size.

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::picking::hover::HoverMap;

/// Pixels per line for `MouseScrollUnit::Line`. Matches the value used
/// in the official Bevy scroll example.
const SCROLL_LINE_HEIGHT: f32 = 21.0;

/// Entity event fired at each entity the pointer hovers when a
/// `MouseWheel` arrives. Propagates up the UI tree (`auto_propagate`)
/// so the event reaches the nearest scrollable ancestor even if the
/// pointer is over a non-scrollable child (e.g. row text).
///
/// `delta` is in pixels, sign-flipped so positive y scrolls content
/// downward (matches typical OS conventions for "scroll down").
#[derive(EntityEvent, Debug)]
#[entity_event(propagate, auto_propagate)]
pub struct Scroll {
    pub entity: Entity,
    pub delta: Vec2,
}

/// Read `MouseWheel` events each frame, normalise to pixels, and fire
/// a `Scroll` entity event at every hovered entity. The event's
/// propagation finds the scrollable ancestor.
///
/// `HoverMap` is `Option<>` so the system no-ops cleanly in headless
/// tests that use `MinimalPlugins` without `bevy::picking`. The
/// production app has it via `DefaultPlugins`.
pub fn send_scroll_events(
    mut reader: MessageReader<MouseWheel>,
    hover_map: Option<Res<HoverMap>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
) {
    let Some(hover_map) = hover_map else {
        reader.clear();
        return;
    };
    for ev in reader.read() {
        // Sign-flipped: wheel-down = positive y delta = scroll content down.
        let mut delta = -Vec2::new(ev.x, ev.y);
        if ev.unit == MouseScrollUnit::Line {
            delta *= SCROLL_LINE_HEIGHT;
        }
        // Hold Ctrl/Cmd to swap axes — lets a vertical wheel scroll a
        // horizontally-scrollable container. Standard convention.
        if keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]) {
            std::mem::swap(&mut delta.x, &mut delta.y);
        }
        for pointer_map in hover_map.values() {
            for entity in pointer_map.keys().copied() {
                commands.trigger(Scroll { entity, delta });
            }
        }
    }
}

/// Observer: when a `Scroll` arrives at an entity that has an
/// `Overflow::scroll_*` axis, apply the delta to its `ScrollPosition`
/// (clamped to content size) and stop propagation if the delta was
/// fully consumed. If the entity isn't scrollable on the relevant axis
/// or is already at the end, the event keeps propagating up to find a
/// container that can scroll.
pub fn on_scroll(
    mut scroll: On<Scroll>,
    mut q: Query<(&mut ScrollPosition, &Node, &ComputedNode)>,
) {
    let Ok((mut pos, node, computed)) = q.get_mut(scroll.entity) else {
        return;
    };
    let max = (computed.content_size() - computed.size()) * computed.inverse_scale_factor();

    if node.overflow.y == OverflowAxis::Scroll && scroll.delta.y != 0.0 {
        let at_end = if scroll.delta.y > 0.0 {
            pos.y >= max.y
        } else {
            pos.y <= 0.0
        };
        if !at_end {
            pos.y = (pos.y + scroll.delta.y).clamp(0.0, max.y.max(0.0));
            scroll.delta.y = 0.0;
        }
    }
    if node.overflow.x == OverflowAxis::Scroll && scroll.delta.x != 0.0 {
        let at_end = if scroll.delta.x > 0.0 {
            pos.x >= max.x
        } else {
            pos.x <= 0.0
        };
        if !at_end {
            pos.x = (pos.x + scroll.delta.x).clamp(0.0, max.x.max(0.0));
            scroll.delta.x = 0.0;
        }
    }
    if scroll.delta == Vec2::ZERO {
        scroll.propagate(false);
    }
}

/// Parameters for one button in an overlay modal.
pub struct ModalButton<M: Component> {
    pub label: &'static str,
    pub marker: M,
    pub disabled: bool,
}

/// Spawn a full-screen absolute overlay with a centred body column and a
/// button row. Returns the Entity of the backdrop node so callers can
/// parent it and attach their own markers.
pub fn spawn_overlay_modal<M: Component>(
    commands: &mut Commands,
    headline: &str,
    body_text: Option<&str>,
    buttons: Vec<ModalButton<M>>,
) -> Entity {
    let backdrop = commands
        .spawn((
            Name::new("modal_backdrop"),
            Node {
                width: percent(100),
                height: percent(100),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: px(24),
                ..default()
            },
            BackgroundColor(COLOR_OVERLAY),
        ))
        .id();

    commands.entity(backdrop).with_children(|parent| {
        parent.spawn((
            Text::new(headline.to_string()),
            TextFont {
                font_size: FONT_HEADER,
                ..default()
            },
            TextColor(COLOR_TEXT),
            Node {
                margin: UiRect::bottom(px(8)),
                ..default()
            },
        ));
        if let Some(body) = body_text {
            parent.spawn((
                Text::new(body.to_string()),
                TextFont {
                    font_size: FONT_BODY,
                    ..default()
                },
                TextColor(COLOR_TEXT_DIM),
                Node {
                    margin: UiRect::bottom(px(16)),
                    ..default()
                },
            ));
        }
        parent
            .spawn((Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(16),
                ..default()
            },))
            .with_children(|row| {
                for btn_spec in buttons {
                    let (bg, text_color) = if btn_spec.disabled {
                        (COLOR_BUTTON_DISABLED, COLOR_TEXT_DIM)
                    } else {
                        (COLOR_BUTTON, COLOR_TEXT)
                    };
                    let mut btn = row.spawn((
                        Button,
                        btn_spec.marker,
                        Node {
                            width: px(200),
                            height: px(56),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        BackgroundColor(bg),
                    ));
                    if btn_spec.disabled {
                        btn.insert(ButtonDisabled);
                    }
                    btn.with_child((
                        Text::new(btn_spec.label),
                        TextFont {
                            font_size: FONT_BUTTON,
                            ..default()
                        },
                        TextColor(text_color),
                    ));
                }
            });
    });

    backdrop
}
