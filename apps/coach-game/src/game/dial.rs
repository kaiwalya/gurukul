//! InGame dial: a note dial anchored on **Sa (the song's tonic)**,
//! tracking the coach's live `f0` as the primary needle. North (12
//! o'clock, angle 0) is always Sa, wherever the singer plants it. The
//! dial sizes itself to the tuning's actual slot count N: 12 for 12-TET
//! and Hindustani Just, 22 for the 22-shruti grid.
//!
//! The tuning is **not** chosen here — the dial spawns empty and paints
//! its slots from [`MusicInfoRes`], the read model the coach publishes on
//! `ConfigureSession` (the same snapshot the HUD reads). So the slots
//! reflect the singer's *real* tuning + tonality, not a hardcoded default.
//!
//! **No frequency is hardcoded here, and the dial never touches raw Hz.**
//! The anchor is Sa, resolved inside the [`Scale`] (`pitch_at(0)`); the
//! live pitch arrives as a [`PitchLog2`] from [`LatestFeatures`] (the game
//! lifted it out of Hz at the poll seam). The two local helpers
//! [`tick_angle`]/[`needle_angle`] place a slot and a live pitch on the
//! Sa-anchored octave circle by *composing* the geometry's own operators
//! (`-`, `fract`, `angle`, [`TuningIntervals::cumulative_rotation_to`]) —
//! no new pitch math, only the render read:
//!
//! - **Needle** ([`needle_angle`]): the live pitch folded against Sa, the
//!   same fold the ticks use, so a perfectly-sung Just Pa lands exactly on
//!   the uneven Just Pa tick.
//! - **Tuning ring** ([`tick_angle`]): each slot's within-octave angle
//!   relative to Sa, so a non-uniform tuning keeps its uneven spacing.
//! - **Scale ring** ([`ScaleIntervals::degree_slots`]): which slots are lit
//!   — the set bits of the scale mask, in slot space from Sa = slot 0.
//!
//! Voiced (`pitch` is `Some`) → one primary needle at the detected pitch;
//! unvoiced → no needle (`needles.is_empty()`), which the widget also
//! reads as "no current slot". No smoothing — the pitch comes straight
//! from the latest poll.

use crate::coach::{Coach, Features, LatestFeatures, MusicInfoRes};
use crate::game::InGameRoot;
use crate::state::{AppSettings, SongTonality};
use crate::ui::*;
use crate::widgets::note_dial::{DialScale, DialSlot, DialState, Needle, NeedleStyle};
use bevy::prelude::*;
use domain_ports::app_coach::{Command, MusicInfo};
use domain_ports::pitch::PitchLog2;
use domain_ports::scale::Scale;
use domain_ports::tuning::Tuning;

/// Confidence floor below which "Capture Sa" is disabled — the same
/// periodicity signal the needle brightness uses. Below this the live
/// `f0` is noise/breath, not a pitch worth pinning Sa to.
const CAPTURE_CONF_GATE: f32 = 0.5;

/// Marker for the InGame dial entity so its `DialState` can be looked
/// up each frame without ambiguity.
#[derive(Component)]
pub struct InGameDial;

/// Marker for the dial's **center hub** — a click target over the dial's
/// middle (where Sa lives). Clicking captures the live pitch as the new
/// root; hovering reveals the "Capture Sa" affordance. Gated on confidence
/// (greyed + inert below [`CAPTURE_CONF_GATE`]).
#[derive(Component)]
pub struct DialHub;

/// Marker for the hub's label text, so [`sync_hub`] can swap it between the
/// resting "Sa" and the hover "Capture Sa".
#[derive(Component)]
pub struct DialHubLabel;

/// The dial tick angle of tuning **slot `i`**, in `[0, TAU)`: the cumulative
/// rotation from Sa to slot `i`, read as an angle. Slot 0 (Sa) is 0 (north).
/// Routes through the rotated tuning's gaps, so an uneven tuning (Just,
/// shruti) keeps its uneven tick spacing — Just Pa lands at `log2(3/2)`, not
/// the even `7/12`.
///
/// This is a *render* read, not pitch math: it composes the geometry's own
/// [`TuningIntervals::cumulative_rotation_to`] and
/// [`angle`](domain_ports::pitch::PitchLog2Interval::angle), so the dial
/// stays free of any tuning arithmetic of its own.
///
/// [`TuningIntervals::cumulative_rotation_to`]: domain_ports::tuning::TuningIntervals::cumulative_rotation_to
fn tick_angle(scale: &Scale, i: usize) -> f32 {
    scale.tuning().intervals().cumulative_rotation_to(i).angle()
}

/// Where a **live pitch** lands on the Sa-anchored dial, as a within-octave
/// angle in `[0, TAU)`: Sa at 0 (north), climbing clockwise. The pitch's
/// helical distance from Sa (`scale.pitch_at(0)`), octave-folded
/// ([`fract`](domain_ports::pitch::PitchLog2Interval::fract)) and read as an
/// [`angle`](domain_ports::pitch::PitchLog2Interval::angle).
///
/// The needle: a perfectly-sung degree lands exactly on that degree's
/// [`tick_angle`], because both fold the same helix against the same Sa.
/// Register-free — a voice an octave high reads the same angle as one at
/// Sa's own octave. Takes a [`PitchLog2`] (not Hz): the conversion already
/// happened at the [`LatestFeatures`] seam, so this is pure composition of
/// geometry operators.
fn needle_angle(scale: &Scale, pitch: PitchLog2) -> f32 {
    (pitch - scale.pitch_at(0)).fract().angle()
}

/// Build the N dial slots from a [`MusicInfo`] snapshot's [`Scale`]: each
/// slot `i`'s tick angle from [`tick_angle`] (the within-octave angle
/// relative to Sa, so a non-uniform tuning keeps its uneven spacing and Sa
/// sits at north), each slot's `active` flag from whether `i` is one of the
/// scale's degree slots ([`ScaleIntervals::degree_slots`]). N is the tuning's
/// slot count — 12 for 12-TET / Hindustani Just, 22 for the 22-shruti grid.
///
/// Pulled out so [`spawn`] (shell) and [`repaint_slots`] (paint) share one
/// definition, and so it's unit testable without a Bevy world.
fn build_slots(info: &MusicInfo) -> Vec<DialSlot> {
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

/// Spawn the dial as a bottom-right overlay on InGame entry, **empty**.
/// The slots aren't known yet: the tuning + tonality come from the
/// coach's read model ([`MusicInfoRes`]), which may not have landed on the
/// frame the dial spawns. [`repaint_slots`] fills them in as soon as the
/// snapshot is available (and again whenever it changes). Until then the
/// dial renders as a bare ring with no slots — honest absence, mirroring
/// the HUD's `—` placeholder.
pub fn spawn(mut commands: Commands, root: Single<Entity, With<InGameRoot>>) {
    let dial = commands
        .spawn((
            ChildOf(*root),
            InGameDial,
            // `Button` makes the whole dial box pickable so `sync_hub` can
            // reveal the hub on *dial* hover (not hub hover). `ButtonSelected`
            // opts it out of the generic repaint — it has no `BackgroundColor`
            // to paint anyway, and there's no dial-click handler.
            Button,
            ButtonSelected,
            Node {
                position_type: PositionType::Absolute,
                right: px(80),
                bottom: px(80),
                width: px(324),
                height: px(324),
                // Center the hub child over the dial's middle (Sa).
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            DialScale { slots: Vec::new() },
            DialState::default(),
        ))
        .id();

    // Center hub: the click target that captures the live pitch as the new
    // root. Spawns enabled; `sync_hub` greys it immediately if there's no
    // voiced pitch. Painted by `update_button_colors` for the hover/press
    // feel (it's a plain Button).
    commands.entity(dial).with_child((
        Button,
        DialHub,
        // `ButtonSelected` opts the hub OUT of the generic hover/press
        // repaint (`update_button_colors`): `sync_hub` is its sole painter,
        // driving a three-state look (invisible at rest, visible-disabled or
        // visible-enabled on hover) that the generic pass can't express.
        ButtonSelected,
        Node {
            width: px(64),
            height: px(64),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(percent(50)),
            ..default()
        },
        // Starts transparent — invisible until hovered. The box still picks
        // up `Interaction` (transparency doesn't disable picking), which is
        // exactly what makes hover-to-reveal work.
        BackgroundColor(Color::NONE),
        children![(
            DialHubLabel,
            Text::new("Zero"),
            TextFont {
                font_size: FONT_BODY,
                ..default()
            },
            TextColor(Color::NONE),
        )],
    ));
}

/// Paint the dial's slots from the [`MusicInfoRes`] read model. Writes a
/// fresh [`DialScale`] (which the widget's `rebuild_slots` repaints on via
/// `Changed<DialScale>`) when either:
///
/// - the snapshot just changed (`music.is_changed()` — a new
///   `ConfigureSession` round-tripped through the coach), or
/// - the dial still has no slots (freshly spawned this InGame visit while
///   the resource already held a `Some` from a prior session — no resource
///   change fires, so we detect the empty shell and fill it).
///
/// No `Some` snapshot yet → leave the dial empty; nothing to draw.
pub fn repaint_slots(music: Res<MusicInfoRes>, mut dial: Query<&mut DialScale, With<InGameDial>>) {
    let Some(info) = music.0 else {
        return;
    };
    let Ok(mut scale) = dial.single_mut() else {
        return;
    };
    if !music.is_changed() && !scale.slots.is_empty() {
        return;
    }
    scale.slots = build_slots(&info);
}

/// Each frame, read the latest [`Features`] and update the dial's
/// `DialState.needles`. Voiced (`pitch` is `Some`) → one primary needle at
/// [`needle_angle`] (the live pitch placed relative to Sa); unvoiced
/// (`pitch` is `None`) → empty `needles`. Needs the [`MusicInfoRes`]
/// snapshot for the current tuning + tonic — north is Sa, so the needle is
/// meaningless without it (no snapshot yet → no needle, mirroring the
/// empty-dial spawn state).
///
/// Note: we don't dedupe on `t_ms` here (unlike `log_features`); even
/// if the feature snapshot hasn't advanced, leaving `DialState` with
/// the same contents is idempotent and skipping the write avoids
/// triggering `Changed<DialState>` every frame, which would cause
/// the widget to repaint and respawn needles unnecessarily.
pub fn update_from_features(
    features: Res<LatestFeatures>,
    music: Res<MusicInfoRes>,
    mut dial: Query<&mut DialState, With<InGameDial>>,
) {
    let Ok(mut state) = dial.single_mut() else {
        return;
    };
    let (
        Some(Features {
            pitch: Some(pitch),
            confidence,
            ..
        }),
        Some(info),
    ) = (features.0, music.0)
    else {
        // No snapshot, no music config, or unvoiced frame → ensure no needle.
        if !state.needles.is_empty() {
            state.needles.clear();
        }
        return;
    };

    let angle = needle_angle(&info.scale, pitch);
    // Map YIN confidence to needle brightness, raised to the 4th so
    // low (noise-floor) confidence collapses to invisible when not
    // phonating while confident voice stays solid. No floor — an
    // untrusted pitch should fade out entirely.
    let conf = confidence.clamp(0.0, 1.0);
    let brightness = conf.powi(4);
    // Replace any prior needle. Writing through DerefMut triggers
    // change detection on DialState, which the widget uses to repaint.
    state.needles.clear();
    state.needles.push(Needle {
        angle,
        style: NeedleStyle::Primary,
        brightness,
    });
}

/// Click the center hub → capture the live pitch as the song's tonic (Sa).
///
/// Resolves the live `f0` to the nearest tuning groove (register preserved —
/// the singer's real octave becomes Sa's) via [`TuningAbsolute::resolve`] on
/// the reference-anchored tuning, then rebuilds the [`Scale`] keeping the
/// current tooth pattern (mask) but re-rooting Sa to that slot and register.
/// Writes it to [`SongTonality`] and round-trips it through the coach via
/// `ConfigureSession`. Gated on confidence — a no-op below
/// [`CAPTURE_CONF_GATE`] (the hub is also greyed by [`sync_hub`]; this is the
/// guard).
///
/// [`TuningAbsolute::resolve`]: domain_ports::tuning::TuningAbsolute::resolve
pub fn handle_hub_capture(
    q: Query<&Interaction, (Changed<Interaction>, With<DialHub>)>,
    features: Res<LatestFeatures>,
    settings: Res<AppSettings>,
    mut tonality: ResMut<SongTonality>,
    coach: NonSend<Coach>,
) {
    for i in q.iter() {
        if *i != Interaction::Pressed {
            continue;
        }
        let Some(snap) = features.0 else {
            return;
        };
        let Some(pitch) = snap.pitch else {
            return; // gate: unvoiced, not a trustworthy pitch
        };
        if snap.confidence < CAPTURE_CONF_GATE {
            return; // gate: weakly-voiced
        }
        // Resolve the live pitch → (slot, octave) against the reference-
        // anchored tuning: the inverse of placing a line. The slot becomes
        // the new Sa rotation, the octave its register.
        let absolute = settings.tuning_absolute();
        let (slot, octave) = absolute.resolve(pitch);
        info!(
            "capture-Sa: f0={:.1}Hz conf={:.2} → slot {slot} octave {octave}",
            pitch.to_hz(),
            snap.confidence,
        );
        // Keep the current tooth pattern; re-root Sa to the captured slot.
        let intervals = tonality.0.intervals();
        tonality.0 = Scale::new(intervals, absolute.shift_up(slot), octave);
        coach
            .0
            .send_command(Command::ConfigureSession { scale: tonality.0 });
        return;
    }
}

/// Paint the center hub from **dial hover** + live confidence — `sync_hub`
/// is the hub's sole painter (it carries [`ButtonSelected`] so the generic
/// repaint skips it). Three states:
///
/// - **Not hovering the dial** → fully transparent (invisible, but the box
///   still picks up `Interaction`, so it stays clickable once revealed).
/// - **Hovering, below the confidence gate** → visible but greyed (disabled
///   look; [`handle_hub_capture`] is the actual click guard).
/// - **Hovering, above the gate** → visible + enabled (darkens while
///   pressed for click feedback).
///
/// "Hovering the dial" means the pointer is over the dial box *or* the hub
/// itself — the hub child would otherwise occlude the dial's own hover and
/// make itself vanish exactly as you reach to click it.
pub fn sync_hub(
    dial_q: Query<&Interaction, With<InGameDial>>,
    hub_q: Query<&Interaction, With<DialHub>>,
    features: Res<LatestFeatures>,
    mut bg_q: Query<&mut BackgroundColor, With<DialHub>>,
    mut label_q: Query<&mut TextColor, With<DialHubLabel>>,
) {
    let dial_hovered = dial_q
        .single()
        .map(|i| *i != Interaction::None)
        .unwrap_or(false);
    let hub_interaction = hub_q.single().copied().unwrap_or(Interaction::None);
    let hovered = dial_hovered || hub_interaction != Interaction::None;

    let voiced = features
        .0
        .map(|s| s.pitch.is_some() && s.confidence >= CAPTURE_CONF_GATE)
        .unwrap_or(false);

    // Resolve the three-state look into a (background, text) colour pair.
    let (bg, text) = if !hovered {
        (Color::NONE, Color::NONE) // invisible at rest
    } else if !voiced {
        (COLOR_BUTTON_DISABLED, COLOR_TEXT_DIM) // visible, disabled
    } else if hub_interaction == Interaction::Pressed {
        (COLOR_BUTTON_PRESSED, COLOR_TEXT) // press feedback
    } else {
        (COLOR_BUTTON, COLOR_TEXT) // visible, enabled
    };

    // Only write on change so we don't retrigger change-detection each frame.
    if let Ok(mut color) = bg_q.single_mut() {
        if color.0 != bg {
            color.0 = bg;
        }
    }
    if let Ok(mut color) = label_q.single_mut() {
        if color.0 != text {
            color.0 = text;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::pitch::PitchLog2Interval;
    use domain_ports::scale::ScaleIntervals;
    use domain_ports::tuning::{TuningAbsolute, TuningKind};
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
        // Bilawal, Sa on slot 5 of 12-TET. Sa always sits at angle 0 (north)
        // because the tuning is re-based so its root line *is* Sa.
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
        // Bilawal lights 7 of 12.
        assert_eq!(slots.iter().filter(|s| s.active).count(), 7);
    }

    #[test]
    fn build_slots_just_keeps_uneven_ticks() {
        // Just intonation moves tick *angles* (Pa at true 3/2) while the lit
        // set — a pure mask projection — matches 12-TET's.
        let tet = build_slots(&info(TuningKind::TwelveTet, 0, 8, &BILAWAL));
        let just = build_slots(&info(TuningKind::HindustaniJust, 0, 8, &BILAWAL));
        // Pa (slot 7) angle differs: even 7/12 in 12-TET, log2(3/2) in Just.
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
        // Bilawal on the 22-shruti grid. Widths [3,2,4,4,3,2,4] walk to
        // degree slots {0,3,5,9,13,16,18}.
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
        // Sing exactly Sa → needle at 0 (north).
        let info = info(TuningKind::TwelveTet, 5, 8, &BILAWAL);
        let sa = info.scale.pitch_at(0);
        let a = needle_angle(&info.scale, sa);
        assert!(a.abs() < 1e-3 || (a - TAU).abs() < 1e-3, "got {a}");
    }

    #[test]
    fn needle_lands_on_each_swara_tick() {
        // The core invariant: every Bilawal swara's needle sits exactly on
        // its lit tick — in BOTH tunings (proves it's not a 12-TET fluke).
        for kind in [TuningKind::TwelveTet, TuningKind::HindustaniJust] {
            let info = info(kind, 5, 8, &BILAWAL);
            let scale = info.scale;
            let slots = build_slots(&info);
            // Each scale degree's pitch must produce a needle equal to the
            // angle of the lit slot it lands on.
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
        // A true Just Pa (3/2 above Sa) must land on the Just Pa tick, which
        // sits past the even 7/12 — confirming the needle routes through the
        // real geometry, not a 12-TET assumption.
        let info = info(TuningKind::HindustaniJust, 0, 8, &BILAWAL);
        let scale = info.scale;
        // A true Just Pa: Sa raised by the 3/2 ratio (= +log2(1.5) on the
        // log2 line), built geometrically — no Hz round-trip.
        let pa = scale.pitch_at(0) + PitchLog2Interval((1.5_f32).log2());
        let needle = needle_angle(&scale, pa);
        let just_pa = (1.5_f32).log2().rem_euclid(1.0) * TAU;
        assert!(
            (needle - just_pa).abs() < 1e-3,
            "Just Pa needle {needle} vs {just_pa}"
        );
    }
}
