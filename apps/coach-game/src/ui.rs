//! Shared UI styling for the menu shell.
//!
//! No theming system yet — these constants are the whole palette. When
//! the look starts to mature we'll likely move to a `Theme` resource.

use bevy::prelude::*;

pub const COLOR_BG: Color = Color::srgb(0.08, 0.08, 0.10);
pub const COLOR_BUTTON: Color = Color::srgb(0.18, 0.18, 0.22);
pub const COLOR_BUTTON_HOVER: Color = Color::srgb(0.28, 0.28, 0.34);
pub const COLOR_BUTTON_PRESSED: Color = Color::srgb(0.12, 0.12, 0.16);
pub const COLOR_TEXT: Color = Color::srgb(0.92, 0.92, 0.95);
pub const COLOR_TEXT_DIM: Color = Color::srgb(0.55, 0.55, 0.60);
pub const COLOR_ACCENT: Color = Color::srgb(0.45, 0.70, 0.95);

pub const FONT_TITLE: f32 = 56.0;
pub const FONT_HEADER: f32 = 32.0;
pub const FONT_BUTTON: f32 = 26.0;
pub const FONT_BODY: f32 = 20.0;

/// Repaint the button's background each frame based on its interaction
/// state. Pair with any `Button` entity; the system below runs in
/// `Update` and covers every button regardless of screen.
pub fn update_button_colors(
    mut q: Query<(&Interaction, &mut BackgroundColor), (Changed<Interaction>, With<Button>)>,
) {
    for (interaction, mut bg) in q.iter_mut() {
        *bg = match *interaction {
            Interaction::Pressed => BackgroundColor(COLOR_BUTTON_PRESSED),
            Interaction::Hovered => BackgroundColor(COLOR_BUTTON_HOVER),
            Interaction::None => BackgroundColor(COLOR_BUTTON),
        };
    }
}
