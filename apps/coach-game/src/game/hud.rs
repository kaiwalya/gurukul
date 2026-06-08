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
//! One row describes the scale's shape:
//!   - **int** — the tooth-widths, the gaps between successive degrees
//!     walking up from Sa and closing the octave: `[2 2 1 2 2 2 1]` for
//!     Bilawal (sums to the tuning's slot count). The musical shape,
//!     independent of where Sa sits. This is [`ScaleIntervals::widths`]
//!     against the tuning's slot count.
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
use domain_ports::tuning::Tuning;

/// Marker for the panel container so its row children can be located,
/// refreshed when the snapshot changes, and detected as the click target
/// for the scale picker. The node also carries a [`Button`] component
/// so Bevy's interaction system tracks hover/press on it.
#[derive(Component)]
pub struct HudBadge;

/// The math-view row. Marks the `Text` node whose content `refresh`
/// overwrites.
#[derive(Component)]
pub struct HudDegRow;

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

    // One row. The placeholder string gets overwritten on the frame the
    // snapshot first reads `Some`.
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
}

/// Refresh the three rows from the [`MusicInfoRes`] read model,
/// rewriting only when the snapshot changes (compared against
/// [`LastMusicInfo`]). The coach handle is no longer touched here —
/// `drain_events` republishes `music_info()` into the resource.
pub fn refresh(
    music: Res<MusicInfoRes>,
    mut last: ResMut<LastMusicInfo>,
    mut deg: Query<&mut Text, With<HudDegRow>>,
) {
    let info = music.0;
    if info == last.0 {
        return;
    }
    last.0 = info;

    let deg_s = match info {
        Some(info) => int_row(&info),
        // No session configured yet: show the model is simply absent
        // rather than faking a default. Honest placeholder.
        None => "int —".into(),
    };
    if let Ok(mut t) = deg.single_mut() {
        **t = deg_s;
    }
}

/// Build the int row from a snapshot: the scale's tooth-widths against the
/// tuning's slot count — the gaps between successive degrees, closing the
/// octave (`2 2 1 2 2 2 1` for Bilawal). Pulled out so it's unit testable
/// without a Bevy world.
fn int_row(info: &MusicInfo) -> String {
    let n = info.scale.tuning().len() as u32;
    format!(
        "int {}",
        info.scale
            .intervals()
            .widths(n)
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::pitch::PitchLog2;
    use domain_ports::scale::{Scale, ScaleIntervals};
    use domain_ports::tuning::{TuningAbsolute, TuningKind};

    fn bilawal_a440() -> MusicInfo {
        let tuning = TuningAbsolute::at_reference(
            TuningKind::TwelveTet.intervals(),
            PitchLog2::from_hz(440.0),
        );
        let intervals = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        // Sa on C, one octave below the A=440 reference.
        MusicInfo {
            scale: Scale::new(intervals, tuning.shift_up(3), 8),
        }
    }

    #[test]
    fn intervals_are_the_tooth_widths() {
        // Sa-relative tooth-widths, closing the octave (sum to 12).
        let int = int_row(&bilawal_a440());
        assert_eq!(int, "int 2 2 1 2 2 2 1");
    }
}
