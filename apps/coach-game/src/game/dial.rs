//! InGame dial: a 12-position note dial with C as the root, tracking
//! the coach's live `f0` as the primary needle.
//!
//! Two tunings are available (see [`tuning_12tet`] and
//! [`tuning_hindustani_just`]); the active one is selected at the call
//! site in [`spawn`]. The needle math ([`angle_from_f0`]) is the same
//! for both — the needle just shows "where on the log-frequency circle
//! is this Hz, relative to C". The tunings differ only in *where the
//! 12 slots sit* on that circle (the targets). Switching tuning moves
//! the targets the needle is judged against, not the needle itself.
//!
//! Slot *positions* come from the tuning (12-TET or Hindustani Just);
//! slot *active/inactive* comes from [`SongTonality`] — its scale
//! intervals, walked from the tonic, pick out which slots are in the
//! current scale. Default scale is Bilawal on Safed-1, so 7 of 12 slots
//! render highlighted. Voiced → one primary needle pointing at the
//! detected pitch. Unvoiced → no needle (`needles.is_empty()`), which
//! the widget also reads as "no current slot". No smoothing — raw `f0`
//! straight from the feature stream.
//!
//! **The scale mask is a head-side render projection** ([`in_scale_mask`]).
//! It is tuning-independent — the lit *slots* are the same integer set
//! whether the tuning is 12-TET or Just (the tuning only changes where
//! each slot is *drawn*, via the angle tables below). So the head walks
//! the [`Tonality`]'s intervals itself rather than asking the coach;
//! see `docs/MUSIC_MODEL.md` § "The mask is a head-side projection".

use crate::coach::LatestFeatures;
use crate::state::{AppState, SongTonality};
use crate::widgets::note_dial::{DialScale, DialSlot, DialState, Needle, NeedleStyle};
use bevy::prelude::*;
use domain_ports::app_coach::FeatureSnapshot;
use domain_ports::music::{tuning_view, Tonality, TuningKind};
use std::f32::consts::TAU;

/// Reference frequency for the root (C / Sa) used by [`angle_from_f0`].
/// A=440 Hz concert pitch, C is 9 semitones below A4 → 440 * 2^(-9/12)
/// ≈ 261.6256 Hz.
const C_REF_HZ: f32 = 261.625_56;

/// Marker for the InGame dial entity so its `DialState` can be looked
/// up each frame without ambiguity.
#[derive(Component)]
pub struct InGameDial;

/// Number of dial slots — one chromatic octave. The angle tables and
/// the scale mask are both sized to this. Non-12 tunings (Carnatic
/// 22-shruti) are deferred; when they land this becomes the tuning's
/// slot count rather than a constant.
const SLOT_COUNT: usize = 12;

/// Walk a [`Tonality`]'s key-widths to a 12-slot in-scale mask.
/// Index `i` is `true` iff slot `i` is one of the scale's notes.
///
/// Starts at the tonic's within-octave position (`tonic.offset %
/// SLOT_COUNT`) and adds each key-width modulo [`SLOT_COUNT`], marking
/// every visited slot. The tonic itself is always lit; the final width
/// lands back on it by construction (a well-formed scale's widths sum to
/// the slot count), so it isn't double-counted.
///
/// This is the **scale ring** (layer 4) — pure integer projection of the
/// `Tonality`, tuning-independent. Lives head-side because the head
/// holds the `Tonality`; the coach is not consulted.
pub fn in_scale_mask(tonality: &Tonality) -> [bool; SLOT_COUNT] {
    let mut mask = [false; SLOT_COUNT];
    let mut cursor = tonality.tonic.offset as usize % SLOT_COUNT;
    mask[cursor] = true;
    for width in tonality.widths() {
        cursor = (cursor + width.0 as usize) % SLOT_COUNT;
        mask[cursor] = true;
    }
    mask
}

/// Spawn the dial as a bottom-right overlay on InGame entry. Twelve
/// slots laid out for the chosen tuning, the in-scale subset marked
/// `active = true` by walking [`SongTonality`].
pub fn spawn(mut commands: Commands, tonality: Res<SongTonality>) {
    // Pick the tuning here. Flip to `tuning_hindustani_just()` to
    // render Sa-rooted Just slot positions instead.
    let slot_angles = tuning_12tet();
    let mask = in_scale_mask(&tonality.0);

    let slots = slot_angles
        .into_iter()
        .enumerate()
        .map(|(i, angle)| DialSlot {
            angle,
            label: None,
            active: mask[i],
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
    features: Res<LatestFeatures>,
    mut dial: Query<&mut DialState, With<InGameDial>>,
) {
    let Ok(mut state) = dial.single_mut() else {
        return;
    };
    let Some(FeatureSnapshot { f0_hz, .. }) = features.0 else {
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

/// The 12 slot angles for a tuning kind, built from the canonical
/// frequencies in [`domain_ports::music`].
///
/// Each slot's frequency comes from the model's
/// [`TuningKind::shape`](domain_ports::music::TuningKind::shape) (the one
/// place the 12-TET spacing and the Just 5-limit ratios are defined), and
/// the dial only does the *geometry*: turn each slot's
/// [`octave_position`](domain_ports::music::tuning_view::octave_position)
/// into a clock angle (`× TAU`). So switching a ratio happens in the model
/// and the dial follows automatically — no ratio table lives here.
///
/// Angles are in `[0, TAU)`, clock convention: 0 = 12 o'clock = C / Sa,
/// positive radians clockwise. The reference frequency the model is built
/// from doesn't matter for the *angles* (only ratios survive the
/// octave-position fold), so we build the shape against [`C_REF_HZ`] and
/// read positions relative to the same anchor.
fn slot_angles(kind: TuningKind) -> [f32; SLOT_COUNT] {
    let slots = kind.shape(C_REF_HZ);
    let mut out = [0.0; SLOT_COUNT];
    for (i, a) in out.iter_mut().enumerate() {
        *a = tuning_view::octave_position(slots[i], C_REF_HZ) * TAU;
    }
    out
}

/// 12-tone equal temperament slot angles — each slot 100 cents (one
/// twelfth of the circle) apart. Thin wrapper over [`slot_angles`].
pub fn tuning_12tet() -> [f32; SLOT_COUNT] {
    slot_angles(TuningKind::TwelveTet)
}

/// Hindustani Just-intonation slot angles — the 5-limit swaras placed at
/// their true log-frequency positions (Pa a touch sharper than 12-TET G,
/// shuddha Ga ~14 cents flatter than 12-TET E). Thin wrapper over
/// [`slot_angles`]; the ratios themselves live in the model's
/// [`TuningKind::HindustaniJust`](domain_ports::music::TuningKind).
pub fn tuning_hindustani_just() -> [f32; SLOT_COUNT] {
    slot_angles(TuningKind::HindustaniJust)
}

/// Map a frequency to a dial angle on the log-frequency circle, with
/// C as the root.
///
/// The model's [`octave_position`](domain_ports::music::tuning_view::octave_position)
/// does the log-frequency fold (where the pitch sits *within* an octave,
/// `[0, 1)`); the dial turns that into a clock angle by `× TAU`. Result is
/// in `[0, TAU)`, clock convention: 0 = 12 o'clock = C, positive radians
/// clockwise. Non-positive frequencies fold to 0.
///
/// This is tuning-independent: the needle just shows where the pitch
/// *actually is* on the log-frequency circle. The tuning's job is to
/// say where the *targets* are.
pub fn angle_from_f0(hz: f32) -> f32 {
    tuning_view::octave_position(hz, C_REF_HZ) * TAU
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::music::harmonium_key;

    // --- in_scale_mask (the head-side scale-ring projection) ----------

    #[test]
    fn bilawal_mask_from_root_highlights_seven_slots() {
        // Bilawal on Safed-1 (slot 0) → Sa Re Ga Ma Pa Dha Ni at slots
        // 0, 2, 4, 5, 7, 9, 11. Komal/tivra slots (1, 3, 6, 8, 10) off.
        let t = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]);
        let mask = in_scale_mask(&t);
        let expected = [
            true, false, true, false, true, true, false, true, false, true, false, true,
        ];
        assert_eq!(mask, expected);
        assert_eq!(mask.iter().filter(|b| **b).count(), 7);
    }

    #[test]
    fn bilawal_mask_rotates_with_tonic() {
        // Same Bilawal shape, Sa on Safed-2 (slot 2 / D): visited slots
        // are 2, 4, 6, 7, 9, 11, 1 → mask shifts by 2.
        let t = Tonality::new(harmonium_key(2), &[2, 2, 1, 2, 2, 2, 1]);
        let mask = in_scale_mask(&t);
        let expected = [
            false, true, true, false, true, false, true, true, false, true, false, true,
        ];
        assert_eq!(mask, expected);
    }

    #[test]
    fn mask_folds_tonic_above_one_octave() {
        // A tonic key an octave up (offset 14 = octave 1, slot 2) folds
        // to the same mask as slot 2 — the ring shows one octave.
        let high = Tonality::new(harmonium_key(14), &[2, 2, 1, 2, 2, 2, 1]);
        let low = Tonality::new(harmonium_key(2), &[2, 2, 1, 2, 2, 2, 1]);
        assert_eq!(in_scale_mask(&high), in_scale_mask(&low));
    }

    // --- angle_from_f0 -------------------------------------------------

    #[test]
    fn c_lands_on_zero() {
        assert!(angle_from_f0(C_REF_HZ).abs() < 1e-4);
    }

    #[test]
    fn c_one_octave_up_also_lands_on_zero() {
        let angle = angle_from_f0(C_REF_HZ * 2.0);
        assert!(angle.abs() < 1e-3 || (angle - TAU).abs() < 1e-3);
    }

    #[test]
    fn a_at_440_lands_at_12tet_slot_9() {
        // A is 9 semitones above C in 12-TET.
        let angle = angle_from_f0(440.0);
        let expected = 9.0 * TAU / 12.0;
        assert!(
            (angle - expected).abs() < 1e-3,
            "got {angle}, want {expected}"
        );
    }

    #[test]
    fn g_lands_at_12tet_slot_7() {
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

    // --- tuning_12tet --------------------------------------------------

    #[test]
    fn tuning_12tet_spaces_slots_evenly() {
        let slots = tuning_12tet();
        assert_eq!(slots[0], 0.0);
        for (i, slot) in slots.iter().enumerate().skip(1) {
            let expected = i as f32 * TAU / 12.0;
            assert!(
                (slot - expected).abs() < 1e-5,
                "slot {i}: got {slot}, want {expected}",
            );
        }
    }

    // --- tuning_hindustani_just ---------------------------------------

    #[test]
    fn tuning_just_sa_at_zero() {
        let slots = tuning_hindustani_just();
        assert_eq!(slots[0], 0.0);
    }

    #[test]
    fn tuning_just_pa_at_3_over_2() {
        let slots = tuning_hindustani_just();
        let expected = 1.5_f32.log2() * TAU;
        assert!((slots[7] - expected).abs() < 1e-5);
    }

    #[test]
    fn tuning_just_shuddha_ga_flatter_than_12tet_e() {
        // 5/4 ≈ 386 cents, 12-TET E = 400 cents → Just is ~14 cents flat.
        let just = tuning_hindustani_just()[4];
        let et = tuning_12tet()[4];
        assert!(
            just < et,
            "shuddha Ga (5/4) should sit below the 12-TET E: just={just}, et={et}"
        );
        // Cents difference: (et - just) * (1200 / TAU).
        let cents_diff = (et - just) * (1200.0 / TAU);
        assert!(
            (cents_diff - 13.686).abs() < 0.05,
            "expected ~13.69 cents flat, got {cents_diff}"
        );
    }

    #[test]
    fn tuning_just_tivra_ma_at_45_over_32() {
        let slots = tuning_hindustani_just();
        let expected = (45.0_f32 / 32.0).log2() * TAU;
        assert!((slots[6] - expected).abs() < 1e-5);
    }

    #[test]
    fn tuning_just_slots_strictly_increasing() {
        let slots = tuning_hindustani_just();
        for i in 1..12 {
            assert!(
                slots[i] > slots[i - 1],
                "slot {i} ({}) should be > slot {} ({})",
                slots[i],
                i - 1,
                slots[i - 1]
            );
        }
    }

    #[test]
    fn needle_at_just_pa_lands_on_just_pa_slot() {
        // Sing a true Just Pa (3/2 of C) → needle should sit exactly on
        // the Just Pa slot, *not* on the 12-TET G slot.
        let just_pa_hz = C_REF_HZ * 1.5;
        let needle = angle_from_f0(just_pa_hz);
        let slot = tuning_hindustani_just()[7];
        assert!(
            (needle - slot).abs() < 1e-4,
            "needle {needle} should align with Just Pa slot {slot}"
        );
    }
}
