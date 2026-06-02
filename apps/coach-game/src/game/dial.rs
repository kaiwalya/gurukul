//! InGame dial: a 12-TET note dial with C as the root, tracking the
//! coach's live `f0` as the primary needle.
//!
//! v0 scope: every semitone is active (no raga yet). Voiced → one
//! primary needle pointing at the detected pitch. Unvoiced → no needle
//! (`needles.is_empty()`), which the widget also reads as "no current
//! slot". No smoothing — raw `f0` straight from the feature stream.

use crate::coach::Coach;
use crate::state::AppState;
use crate::widgets::note_dial::{DialScale, DialSlot, DialState, Needle, NeedleStyle};
use bevy::prelude::*;
use domain_ports::app_coach::FeatureSnapshot;
use std::f32::consts::TAU;

/// Reference frequency for note C used by `angle_from_f0`. A=440 Hz
/// concert pitch, C is 9 semitones below A4 → 440 * 2^(-9/12) ≈
/// 261.6256 Hz.
const C_REF_HZ: f32 = 261.625_56;

/// Marker for the InGame dial entity so its `DialState` can be looked
/// up each frame without ambiguity.
#[derive(Component)]
pub struct InGameDial;

/// Spawn the dial as a top-right overlay on InGame entry. Twelve
/// equal-spaced active slots, root at 12 o'clock (C), empty needles.
pub fn spawn(mut commands: Commands) {
    let slots = (0..12)
        .map(|i| DialSlot {
            angle: i as f32 * TAU / 12.0,
            label: None,
            active: true,
        })
        .collect();

    commands.spawn((
        DespawnOnExit(AppState::InGame),
        InGameDial,
        Node {
            position_type: PositionType::Absolute,
            right: px(80),
            bottom: px(80),
            width: px(324),
            height: px(324),
            ..default()
        },
        DialScale { slots },
        DialState::default(),
    ));
}

/// Each frame, read the latest feature snapshot and update the dial's
/// `DialState.needles`. Voiced (`f0_hz > 0`) → one primary needle at
/// `angle_from_f0(f0_hz)`; unvoiced → empty `needles`.
///
/// Note: we don't dedupe on `t_ms` here (unlike `log_features`); even
/// if the feature snapshot hasn't advanced, leaving `DialState` with
/// the same contents is idempotent and skipping the write avoids
/// triggering `Changed<DialState>` every frame, which would cause
/// the widget to repaint and respawn needles unnecessarily.
pub fn update_from_features(
    coach: NonSend<Coach>,
    mut dial: Query<&mut DialState, With<InGameDial>>,
) {
    let Ok(mut state) = dial.single_mut() else {
        return;
    };
    let Some(FeatureSnapshot { f0_hz, .. }) = coach.0.latest_features() else {
        // No snapshot yet (session just started) → ensure no needle.
        if !state.needles.is_empty() {
            state.needles.clear();
        }
        return;
    };

    if f0_hz <= 0.0 {
        if !state.needles.is_empty() {
            state.needles.clear();
        }
        return;
    }

    let angle = angle_from_f0(f0_hz);
    // Replace any prior needle. Writing through DerefMut triggers
    // change detection on DialState, which the widget uses to repaint.
    state.needles.clear();
    state.needles.push(Needle {
        angle,
        style: NeedleStyle::Primary,
    });
}

/// Map a frequency to a dial angle in 12-TET with C as the root.
///
/// 12 * log2(f / C_REF_HZ) gives the number of semitones above C. We
/// take that mod 12 (so octaves fold onto the same circle) and scale
/// to `[0, TAU)` with clock convention: 0 → 12 o'clock (C), positive
/// clockwise. Non-positive frequencies are caller bugs; the function
/// returns 0 for them.
pub fn angle_from_f0(hz: f32) -> f32 {
    if hz <= 0.0 {
        return 0.0;
    }
    let semitones_above_c = 12.0 * (hz / C_REF_HZ).log2();
    let mod_octave = semitones_above_c.rem_euclid(12.0);
    mod_octave * TAU / 12.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_lands_on_zero() {
        assert!(angle_from_f0(C_REF_HZ).abs() < 1e-4);
    }

    #[test]
    fn c_one_octave_up_also_lands_on_zero() {
        let angle = angle_from_f0(C_REF_HZ * 2.0);
        // Mod 12, so two octaves apart fold to the same slot.
        assert!(angle.abs() < 1e-3 || (angle - TAU).abs() < 1e-3);
    }

    #[test]
    fn a_at_440_lands_at_slot_9() {
        // A is 9 semitones above C.
        let angle = angle_from_f0(440.0);
        let expected = 9.0 * TAU / 12.0;
        assert!(
            (angle - expected).abs() < 1e-3,
            "got {angle}, want {expected}"
        );
    }

    #[test]
    fn g_lands_at_slot_7() {
        // G is 7 semitones above C → G4 = C4 * 2^(7/12).
        let g = C_REF_HZ * (7.0_f32 / 12.0).exp2();
        let angle = angle_from_f0(g);
        let expected = 7.0 * TAU / 12.0;
        assert!((angle - expected).abs() < 1e-3);
    }

    #[test]
    fn unvoiced_returns_zero() {
        assert_eq!(angle_from_f0(0.0), 0.0);
        assert_eq!(angle_from_f0(-1.0), 0.0);
    }
}
