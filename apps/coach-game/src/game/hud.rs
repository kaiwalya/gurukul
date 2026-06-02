//! InGame HUD: a top-left corner badge showing the current tonic +
//! scale, so the singer always has a glanceable answer to "what key
//! am I in?".
//!
//! Per the UI design brief: not a full top bar. A single typographic
//! phrase floating at `(left: 32, top: 24)`, tonic in `FONT_HEADER` +
//! `COLOR_TEXT`, scale in `FONT_BODY` + `COLOR_TEXT_DIM`, same
//! baseline (`AlignItems::Baseline`). No panel background, no border —
//! the dial is the active surface, the HUD recedes.
//!
//! The badge text reflects [`AppSettings::note_system`] (Sargam users
//! see "Safed-1 Bilawal" / "Kaali-1 Bilawal" — the harmonium key
//! their Sa is on, plus the scale; Western users see "C Major" / "C♯
//! Major") and [`SongTonality`] (the singer's Sa key + scale-interval
//! shape). See `docs/MUSIC_MODEL.md` § "Note system" Job B for why
//! Sargam announces tonic by harmonium position rather than as "Sa". It
//! rebuilds whenever either resource changes — including the first
//! frame, via `Added<HudBadge>`, since both resources are initialised
//! at startup and `is_changed()` is `false` by InGame entry.

use crate::state::{scale_name, AppSettings, AppState, SongTonality};
use crate::ui::*;
use bevy::prelude::*;

/// Marker for the badge container so its text children can be located
/// and refreshed when the underlying resources change.
#[derive(Component)]
pub struct HudBadge;

/// Marker for the tonic text node inside the badge (the larger,
/// brighter token — e.g. "Sa" / "C").
#[derive(Component)]
pub struct HudTonicText;

/// Marker for the scale text node inside the badge (the smaller,
/// dimmer token — e.g. "Bilawal" / "Major").
#[derive(Component)]
pub struct HudScaleText;

pub fn spawn(mut commands: Commands) {
    // Row layout with baseline alignment so the smaller scale text
    // sits on the same baseline as the larger tonic — the design
    // explicitly calls for this rather than centre-aligning the box.
    let badge = commands
        .spawn((
            DespawnOnExit(AppState::InGame),
            HudBadge,
            Node {
                position_type: PositionType::Absolute,
                left: px(32),
                top: px(24),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Baseline,
                column_gap: px(10),
                ..default()
            },
        ))
        .id();

    // Both text children get placeholder strings; `refresh` overwrites
    // them on the same frame via the `Added<HudBadge>` trigger.
    commands.spawn((
        ChildOf(badge),
        HudTonicText,
        Text::new(""),
        TextFont {
            font_size: FONT_HEADER,
            ..default()
        },
        TextColor(COLOR_TEXT),
    ));
    commands.spawn((
        ChildOf(badge),
        HudScaleText,
        Text::new(""),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT_DIM),
    ));
}

/// Refresh the badge's two text nodes whenever `AppSettings` or
/// `SongTonality` changes, or when the badge is freshly spawned.
pub fn refresh(
    settings: Res<AppSettings>,
    tonality: Res<SongTonality>,
    mut tonic: Query<&mut Text, (With<HudTonicText>, Without<HudScaleText>)>,
    mut scale: Query<&mut Text, (With<HudScaleText>, Without<HudTonicText>)>,
    just_added: Query<Entity, Added<HudBadge>>,
) {
    if !settings.is_changed() && !tonality.is_changed() && just_added.is_empty() {
        return;
    }
    let song = &tonality.0;
    if let Ok(mut t) = tonic.single_mut() {
        // Job B: name the tonic by its within-octave key position
        // (Safed-2 / D). `fold()` drops any octave so the label table
        // (one octave wide) indexes correctly.
        **t = settings
            .note_system
            .tonic_label(song.tonic.fold())
            .to_string();
    }
    if let Ok(mut t) = scale.single_mut() {
        **t = scale_name(song.steps(), settings.note_system);
    }
}
