//! Note-dial model: the pure domain → geometry projection.
//!
//! The only music-aware layer of the slice. It folds frequencies,
//! scales, and tonics into the dial's geometry — slot angles, needle
//! angles, slot activity, and the hub's visual state. Plain Rust, no
//! Bevy, no `bevy::Color`. After this runs, music has been spent.
//!
//! North (angle 0) is always **Sa** (the song's tonic), wherever the
//! singer plants it. The two local helpers [`tick_angle`] / [`needle_angle`]
//! place a slot and a live pitch on the Sa-anchored octave circle by
//! *composing* the geometry's own operators — no new pitch math.
//!
//! - **Needle** ([`needle_angle`]): the live pitch folded against Sa, the
//!   same fold the ticks use, so a perfectly-sung Just Pa lands exactly on
//!   the uneven Just Pa tick.
//! - **Tuning ring** ([`tick_angle`]): each slot's within-octave angle
//!   relative to Sa, so a non-uniform tuning keeps its uneven spacing.
//! - **Scale ring** ([`ScaleIntervals::degree_slots`]): which slots are lit
//!   — the set bits of the scale mask, in slot space from Sa = slot 0.

use domain_ports::app_coach::MusicInfo;
use domain_ports::pitch::PitchLog2;
use domain_ports::scale::Scale;
use domain_ports::tuning::{Tuning, TuningAbsolute};

use super::scene::{DialSlot, HubState, Needle, NeedleStyle};

/// Confidence floor below which "Capture Sa" is disabled — the same
/// periodicity signal the needle brightness uses. Below this the live
/// `f0` is noise/breath, not a pitch worth pinning Sa to.
pub const CAPTURE_CONF_GATE: f32 = 0.5;

/// The dial tick angle of tuning **slot `i`**, in `[0, TAU)`: the cumulative
/// rotation from Sa to slot `i`, read as an angle. Slot 0 (Sa) is 0 (north).
/// Routes through the rotated tuning's gaps, so an uneven tuning (Just,
/// shruti) keeps its uneven tick spacing — Just Pa lands at `log2(3/2)`, not
/// the even `7/12`.
fn tick_angle(scale: &Scale, i: usize) -> f32 {
    scale.tuning().intervals().cumulative_rotation_to(i).angle()
}

/// Where a **live pitch** lands on the Sa-anchored dial, as a within-octave
/// angle in `[0, TAU)`: Sa at 0 (north), climbing clockwise. The pitch's
/// helical distance from Sa (`scale.pitch_at(0)`), octave-folded and read as
/// an angle. A perfectly-sung degree lands exactly on that degree's
/// [`tick_angle`]. Register-free. Takes a [`PitchLog2`] (not Hz).
fn needle_angle(scale: &Scale, pitch: PitchLog2) -> f32 {
    (pitch - scale.pitch_at(0)).fract().angle()
}

/// Build the N dial slots from a [`MusicInfo`] snapshot's [`Scale`]: each
/// slot `i`'s tick angle from [`tick_angle`], each slot's `active` flag from
/// whether `i` is one of the scale's degree slots. N is the tuning's slot
/// count — 12 for 12-TET / Hindustani Just, 22 for the 22-shruti grid.
pub fn build_slots(info: &MusicInfo) -> Vec<DialSlot> {
    let scale = info.scale;
    let n = scale.tuning().len();
    let lit: Vec<u32> = scale.intervals().degree_slots();
    (0..n)
        .map(|i| DialSlot {
            angle: tick_angle(&scale, i),
            label: None,
            active: lit.contains(&(i as u32)),
        })
        .collect()
}

/// Project a live feature frame onto the dial's primary needle. Voiced
/// (`pitch` is `Some`) → one primary needle at [`needle_angle`] with
/// brightness from confidence; unvoiced / no pitch → `None` (no needle).
///
/// Confidence maps to brightness raised to the 4th, so low (noise-floor)
/// confidence collapses to invisible while confident voice stays solid.
/// No floor — an untrusted pitch fades out entirely.
pub fn project_needle(
    info: &MusicInfo,
    pitch: Option<PitchLog2>,
    confidence: f32,
) -> Option<Needle> {
    let pitch = pitch?;
    let angle = needle_angle(&info.scale, pitch);
    let conf = confidence.clamp(0.0, 1.0);
    Some(Needle {
        angle,
        style: NeedleStyle::Primary,
        brightness: conf.powi(4),
    })
}

/// Capture the live pitch as the song's new Sa: resolve it to the nearest
/// tuning groove (register preserved) against the reference-anchored
/// `absolute` tuning, then rebuild the [`Scale`] keeping the current tooth
/// pattern (`current`'s mask) but re-rooting Sa to that slot and register.
///
/// Pure: the confidence gate and command round-trip stay in glue.
pub fn capture_scale(absolute: &TuningAbsolute, current: &Scale, pitch: PitchLog2) -> Scale {
    let (slot, octave) = absolute.resolve(pitch);
    Scale::new(current.intervals(), absolute.shift_up(slot), octave)
}

/// Resolve the hub's three-state look from dial hover, hub press, and
/// whether the live pitch is voiced above the confidence gate. The
/// [systems](super::systems) layer maps the returned [`HubState`] to colours.
pub fn hub_visual_state(dial_hovered: bool, hub_pressed: bool, voiced: bool) -> HubState {
    if !dial_hovered {
        HubState::Hidden
    } else if !voiced {
        HubState::Disabled
    } else if hub_pressed {
        HubState::Pressed
    } else {
        HubState::Enabled
    }
}

/// Whether a feature frame's pitch is voiced strongly enough to capture Sa
/// (some pitch, confidence at or above [`CAPTURE_CONF_GATE`]).
pub fn is_capture_voiced(pitch: Option<PitchLog2>, confidence: f32) -> bool {
    pitch.is_some() && confidence >= CAPTURE_CONF_GATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::pitch::PitchLog2Interval;
    use domain_ports::scale::ScaleIntervals;
    use domain_ports::tuning::{Tuning, TuningKind};
    use std::f32::consts::TAU;

    const BILAWAL: [u32; 7] = [2, 2, 1, 2, 2, 2, 1];

    /// A reference-anchored A=440 tuning of the given kind.
    fn absolute(kind: TuningKind) -> TuningAbsolute {
        TuningAbsolute::at_reference(kind.intervals(), PitchLog2::from_hz(440.0))
    }

    /// A `MusicInfo` whose `Scale` is `widths` rooted `sa_shift` slots above
    /// the A=440 reference, at register `octave`.
    fn info(kind: TuningKind, sa_shift: usize, octave: i32, widths: &[u32]) -> MusicInfo {
        let intervals = ScaleIntervals::from_widths(widths);
        MusicInfo {
            scale: Scale::new(intervals, absolute(kind).shift_up(sa_shift), octave),
        }
    }

    // --- build_slots: Sa at north, ticks slot-indexed, mask matches ----

    #[test]
    fn build_slots_puts_sa_at_north() {
        let slots = build_slots(&info(TuningKind::TwelveTet, 5, 8, &BILAWAL));
        assert_eq!(slots.len(), 12);
        let north = slots[0].angle;
        assert!(
            north.abs() < 1e-4 || (north - TAU).abs() < 1e-4,
            "Sa at north: {north}"
        );
        assert!(slots[0].active, "Sa slot lit");
        // Pa is the 7th groove up from Sa → slot 7, at 7/12 of a turn.
        assert!(
            (slots[7].angle - 7.0 * TAU / 12.0).abs() < 1e-4,
            "Pa at 7/12"
        );
        assert!(slots[7].active, "Pa slot lit");
    }

    #[test]
    fn build_slots_active_matches_the_degree_slots() {
        let info = info(TuningKind::TwelveTet, 5, 8, &BILAWAL);
        let slots = build_slots(&info);
        let lit = info.scale.intervals().degree_slots();
        for (i, slot) in slots.iter().enumerate() {
            assert_eq!(slot.active, lit.contains(&(i as u32)), "slot {i} active");
            assert!(slot.label.is_none(), "slot {i} label — head is vocab-free");
        }
        assert_eq!(slots.iter().filter(|s| s.active).count(), 7);
    }

    #[test]
    fn build_slots_just_keeps_uneven_ticks() {
        let tet = build_slots(&info(TuningKind::TwelveTet, 0, 8, &BILAWAL));
        let just = build_slots(&info(TuningKind::HindustaniJust, 0, 8, &BILAWAL));
        assert!(
            (tet[7].angle - just[7].angle).abs() > 1e-4,
            "Just must move a tick angle off the even grid"
        );
        for i in 0..12 {
            assert_eq!(tet[i].active, just[i].active, "slot {i} active set");
        }
    }

    #[test]
    fn twenty_two_shruti_has_22_slots_and_7_lit() {
        let slots = build_slots(&info(
            TuningKind::TwentyTwoShruti,
            0,
            8,
            &[3, 2, 4, 4, 3, 2, 4],
        ));
        assert_eq!(slots.len(), 22);
        let lit: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.active)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(lit, vec![0, 3, 5, 9, 13, 16, 18]);
    }

    // --- needle: live pitch placed relative to Sa ----------------------

    #[test]
    fn needle_sa_lands_at_north() {
        let info = info(TuningKind::TwelveTet, 5, 8, &BILAWAL);
        let sa = info.scale.pitch_at(0);
        let a = needle_angle(&info.scale, sa);
        assert!(a.abs() < 1e-3 || (a - TAU).abs() < 1e-3, "got {a}");
    }

    #[test]
    fn needle_lands_on_each_swara_tick() {
        for kind in [TuningKind::TwelveTet, TuningKind::HindustaniJust] {
            let info = info(kind, 5, 8, &BILAWAL);
            let scale = info.scale;
            let slots = build_slots(&info);
            let degrees = scale.intervals().note_count();
            for d in 0..degrees {
                let pitch = scale.pitch_at(d);
                let needle = needle_angle(&scale, pitch);
                let slot = scale
                    .intervals()
                    .slot_of_degree(d, scale.tuning().len() as u32)
                    as usize;
                assert!(slots[slot].active, "swara slot {slot} must be lit");
                let tick = slots[slot].angle;
                let near = (needle - tick).abs() < 1e-3 || (needle - tick).abs() > TAU - 1e-3;
                assert!(near, "needle {needle} vs tick {tick} at slot {slot}");
            }
        }
    }

    #[test]
    fn needle_just_pa_lands_on_just_pa_not_12tet_g() {
        let info = info(TuningKind::HindustaniJust, 0, 8, &BILAWAL);
        let scale = info.scale;
        let pa = scale.pitch_at(0) + PitchLog2Interval((1.5_f32).log2());
        let needle = needle_angle(&scale, pa);
        let just_pa = (1.5_f32).log2().rem_euclid(1.0) * TAU;
        assert!(
            (needle - just_pa).abs() < 1e-3,
            "Just Pa needle {needle} vs {just_pa}"
        );
    }

    // --- project_needle: voiced / unvoiced -----------------------------

    #[test]
    fn project_needle_none_when_unvoiced() {
        let info = info(TuningKind::TwelveTet, 5, 8, &BILAWAL);
        assert!(project_needle(&info, None, 0.9).is_none());
    }

    #[test]
    fn project_needle_voiced_is_primary_at_pitch_angle() {
        let info = info(TuningKind::TwelveTet, 5, 8, &BILAWAL);
        let sa = info.scale.pitch_at(0);
        let needle = project_needle(&info, Some(sa), 1.0).expect("voiced → needle");
        assert!(matches!(needle.style, NeedleStyle::Primary));
        assert!(
            needle.angle.abs() < 1e-3 || (needle.angle - TAU).abs() < 1e-3,
            "Sa needle at north, got {}",
            needle.angle
        );
        assert!((needle.brightness - 1.0).abs() < 1e-6);
    }

    // --- hub_visual_state: the four states -----------------------------

    #[test]
    fn hub_state_distinguishes_all_four() {
        assert_eq!(hub_visual_state(false, false, true), HubState::Hidden);
        assert_eq!(hub_visual_state(false, true, true), HubState::Hidden);
        assert_eq!(hub_visual_state(true, false, false), HubState::Disabled);
        assert_eq!(hub_visual_state(true, false, true), HubState::Enabled);
        assert_eq!(hub_visual_state(true, true, true), HubState::Pressed);
    }

    // --- capture_scale: re-roots Sa, keeps the mask --------------------

    #[test]
    fn capture_scale_keeps_mask_reroots_sa() {
        let absolute = absolute(TuningKind::TwelveTet);
        let current = Scale::new(
            ScaleIntervals::from_widths(&BILAWAL),
            absolute.shift_up(0),
            8,
        );
        // Capture A=440 itself → Sa re-roots onto the reference slot.
        let captured = capture_scale(&absolute, &current, PitchLog2::from_hz(440.0));
        assert_eq!(
            captured.intervals().widths(12),
            current.intervals().widths(12),
            "tooth pattern preserved"
        );
    }
}
