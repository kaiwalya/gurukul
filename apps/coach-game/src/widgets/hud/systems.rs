//! HUD systems: Bevy node spawning, markers, text sync from
//! [`HudSceneRes`](super::scene::HudSceneRes). Knows the engine, not the
//! domain.

use bevy::prelude::*;

use crate::ui::*;

use super::scene::HudSceneRes;

/// Marker for the panel container so its row children can be located and
/// detected as the click target for the scale picker. Carries a [`Button`]
/// so Bevy's interaction system tracks hover/press on it.
#[derive(Component)]
pub struct HudBadge;

/// The math-view row. Marks the `Text` node whose content [`sync_text`]
/// overwrites.
#[derive(Component)]
pub struct HudDegRow;

/// Spawn the HUD badge and its row under `parent`, returning the badge
/// entity. The row's placeholder string is overwritten by [`sync_text`] on
/// the frame the scene first carries content.
pub fn spawn(commands: &mut Commands, parent: Entity) -> Entity {
    let panel = commands
        .spawn((
            ChildOf(parent),
            HudBadge,
            Name::new("hud"),
            Button,
            Node {
                position_type: PositionType::Absolute,
                left: px(32),
                top: px(24),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                padding: UiRect::all(px(6)),
                ..default()
            },
        ))
        .id();

    commands.entity(panel).with_child((
        HudDegRow,
        Text::new(""),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT),
    ));

    panel
}

/// Sync the math-view row's text from [`HudSceneRes`], rewriting only when
/// the scene changes (its `Changed` tracking). Glue forces a scene refresh
/// on InGame entry so a re-entry with identical music info still repaints.
pub fn sync_text(scene: Res<HudSceneRes>, mut deg: Query<&mut Text, With<HudDegRow>>) {
    if !scene.is_changed() {
        return;
    }
    if let Ok(mut t) = deg.single_mut() {
        if t.as_str() != scene.deg_row {
            **t = scene.deg_row.clone();
        }
    }
}
