//! InGame HUD: a top-left corner panel showing the **math view** of
//! the current tonality, so the singer always has a glanceable, honest
//! answer to "what am I singing against?".
//!
//! Why a math view and not note names: the head is deliberately
//! vocabulary-free (see `docs/MUSIC_MODEL.md`). Naming the dial face or
//! the tonic ("Sa", "C", "Safed-2 Bilawal") is a separate, deferred
//! label layer. Until it ships we show the raw numbers the model
//! actually computes — no invented names to drift out of sync.
//!
//! Three parallel rows describe the same scale, each a different view
//! of the same degrees:
//!   - **deg** — 0-based prefix sum of the scale intervals, i.e. the
//!     Sa-relative semitone of each degree: `[0, 2, 4, 5, 7, 9, 11]`
//!     for Bilawal. The musical shape, independent of where Sa sits.
//!   - **key** — those degrees placed onto the keyboard by adding the
//!     tonic's offset: `InstrumentKey` space.
//!   - **Hz** — each key resolved to a frequency through the active
//!     `Tuning` (so 12-TET vs Hindustani Just actually shows).
//!
//! Source of truth is [`AppCoach::music_info`] — the snapshot the
//! coach publishes on `ConfigureSession`. Reading it here (rather than
//! the head's own `SongTonality`) exercises the snapshot round-trip:
//! what the HUD draws is precisely what a fold of the event stream
//! would reconstruct. The panel rebuilds whenever that snapshot
//! changes (tracked via [`LastMusicInfo`]), including the first frame
//! it becomes `Some`.

use crate::coach::MusicInfoRes;
use crate::state::AppState;
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::app_coach::MusicInfo;
use domain_ports::music::{tuning_view, InstrumentKey, ScaleNote, Tuning};

/// Marker for the panel container so its row children can be located
/// and refreshed when the snapshot changes.
#[derive(Component)]
pub struct HudBadge;

/// The three math-view rows, in render order. Each marks one `Text`
/// node whose content `refresh` overwrites.
#[derive(Component)]
pub struct HudDegRow;
#[derive(Component)]
pub struct HudKeyRow;
#[derive(Component)]
pub struct HudHzRow;

/// Last `MusicInfo` the HUD rendered, so `refresh` only rewrites text
/// when the snapshot actually changes. `None` until the first
/// `ConfigureSession` snapshot lands. Lives only for the InGame screen.
#[derive(Resource, Default)]
pub struct LastMusicInfo(pub Option<MusicInfo>);

pub fn spawn(mut commands: Commands, mut last: ResMut<LastMusicInfo>) {
    // Force a repaint on (re)entry: the panel's text nodes spawn empty,
    // so clear the change-tracker even if the snapshot is unchanged
    // from the previous InGame visit.
    last.0 = None;
    let panel = commands
        .spawn((
            DespawnOnExit(AppState::InGame),
            HudBadge,
            Node {
                position_type: PositionType::Absolute,
                left: px(32),
                top: px(24),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                ..default()
            },
        ))
        .id();

    // Three rows, brightest first. Monospaced columns aren't available
    // (default font isn't mono), so each row is a single string the
    // refresh pass formats. Placeholder strings get overwritten on the
    // frame the snapshot first reads `Some`.
    commands.spawn((
        ChildOf(panel),
        HudDegRow,
        Text::new(""),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT),
    ));
    commands.spawn((
        ChildOf(panel),
        HudKeyRow,
        Text::new(""),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT_DIM),
    ));
    commands.spawn((
        ChildOf(panel),
        HudHzRow,
        Text::new(""),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT_DIM),
    ));
}

/// Refresh the three rows from the [`MusicInfoRes`] read model,
/// rewriting only when the snapshot changes (compared against
/// [`LastMusicInfo`]). The coach handle is no longer touched here —
/// `drain_events` republishes `music_info()` into the resource.
pub fn refresh(
    music: Res<MusicInfoRes>,
    mut last: ResMut<LastMusicInfo>,
    mut deg: Query<&mut Text, (With<HudDegRow>, Without<HudKeyRow>, Without<HudHzRow>)>,
    mut key: Query<&mut Text, (With<HudKeyRow>, Without<HudDegRow>, Without<HudHzRow>)>,
    mut hz: Query<&mut Text, (With<HudHzRow>, Without<HudDegRow>, Without<HudKeyRow>)>,
) {
    let info = music.0;
    if info == last.0 {
        return;
    }
    last.0 = info;

    let (deg_s, key_s, hz_s) = match info {
        Some(info) => rows(&info),
        // No session configured yet: show the model is simply absent
        // rather than faking a default. Honest placeholder.
        None => ("deg —".into(), "key —".into(), "Hz  —".into()),
    };
    if let Ok(mut t) = deg.single_mut() {
        **t = deg_s;
    }
    if let Ok(mut t) = key.single_mut() {
        **t = key_s;
    }
    if let Ok(mut t) = hz.single_mut() {
        **t = hz_s;
    }
}

/// Build the three row strings from a snapshot. Pulled out so it's unit
/// testable without a Bevy world.
fn rows(info: &MusicInfo) -> (String, String, String) {
    let tonality = info.tonality;
    let tuning = Tuning::new(info.tuning);

    // One entry per *scale note*. `widths()` holds every key-width
    // including the final one that closes back to Sa an octave up; the
    // notes themselves are `ScaleNote(0)..ScaleNote(n-1)` — Sa..Ni for
    // Bilawal — so we walk `0..widths.len()`, which drops that redundant
    // octave Sa.
    let note_count = tonality.widths().len();
    let keys: Vec<InstrumentKey> = (0..note_count as u8)
        .map(|d| tonality.key_of(ScaleNote { offset: d }))
        .collect();

    // The Sa-relative semitone of each note: `key − tonic` (Bilawal →
    // `0 2 4 5 7 9 11`). The keys here are whole (scale notes land on
    // keyboard keys), so `{:.0}` renders them as plain integers — the
    // fractional slide never reaches this math view.
    let deg_s = format!(
        "deg {}",
        keys.iter()
            .map(|&k| format!("{:.0}", (k - tonality.tonic).0))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let key_s = format!(
        "key {}",
        keys.iter()
            .map(|k| format!("{:.0}", k.offset))
            .collect::<Vec<_>>()
            .join(" ")
    );

    // Hz of each key through the tuning. `tuning_view::hz` derives slot
    // and octave from the same root-relative delta, so degrees below the
    // tuning root read an octave down and the row ascends from Sa
    // naturally — no octave-lifting needed at the call site.
    let hz_s = format!(
        "Hz  {}",
        keys.iter()
            .map(|&k| format!("{:.0}", tuning_view::hz(&tuning, k)))
            .collect::<Vec<_>>()
            .join(" ")
    );

    (deg_s, key_s, hz_s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::music::{harmonium_key, Tonality, TuningKind, TuningSpec};

    fn bilawal_a440() -> MusicInfo {
        MusicInfo {
            tuning: TuningSpec {
                root_note_hz: 440.0,
                kind: TuningKind::TwelveTet,
                // A in octave 1.
                root: harmonium_key(21.0),
            },
            // Bilawal (major) on C in octave 1 — one octave below the
            // root, so the scale lands in the middle register.
            tonality: Tonality::new(harmonium_key(12.0), &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0]),
        }
    }

    #[test]
    fn degrees_are_zero_based_prefix_sum() {
        // Sa-relative, independent of where the tonic sits.
        let (deg, _, _) = rows(&bilawal_a440());
        assert_eq!(deg, "deg 0 2 4 5 7 9 11");
    }

    #[test]
    fn keys_offset_degrees_by_tonic() {
        // Tonic on C octave 1 (offset 12) → keys = 12 + each degree.
        let (_, key, _) = rows(&bilawal_a440());
        assert_eq!(key, "key 12 14 16 17 19 21 23");
    }

    #[test]
    fn hz_row_ascends_from_sa() {
        // 12-TET rooted at A=440 in octave 1. Sa on C one octave below
        // the root resolves to the middle register: a clean C-major
        // octave (C ≈ 262 → B ≈ 494), ascending the whole way, with the
        // root A landing exactly on 440.
        let (_, _, hz) = rows(&bilawal_a440());
        assert_eq!(hz, "Hz  262 294 330 349 392 440 494");
    }
}
