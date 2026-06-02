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
