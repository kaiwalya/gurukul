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
//! **No frequency is hardcoded here.** The anchor is the tonic, which is
//! already in the data ([`Tonality::tonic`]). All three rotating layers
//! reduce to "a position measured from Sa", and the dial does only the
//! render step (`× TAU`); the pitch-math lives in `music.rs`:
//!
//! - **Needle** ([`needle_angle`]): the live `f0` folded against **Sa's
//!   Hz** ([`tuning_view::octave_position`]) — the same Hz fold the ticks
//!   use, so a perfectly-sung Just Pa lands exactly on the uneven Just Pa
//!   tick. The needle shows where the voice actually is in log-frequency.
//! - **Tuning ring** ([`build_slots`] via [`tuning_view::slot_position_from`]):
//!   each slot's *real Hz* folded against Sa's Hz, so a non-uniform tuning
//!   (Just) keeps its uneven tick spacing. The Hz never leaves the view.
//! - **Scale ring** ([`in_scale_mask`]): which slots are lit, walked in
//!   **slot space** from the tonic's slot index `(tonic − root) mod N`.
//!
//! Voiced (`f0_hz > 0`) → one primary needle at the detected pitch;
//! unvoiced → no needle (`needles.is_empty()`), which the widget also
//! reads as "no current slot". No smoothing — raw `f0` from the stream.
//!
//! **The scale mask is a head-side render projection** ([`in_scale_mask`]):
//! the head holds the [`Tonality`] and walks the widths itself rather than
//! asking the coach; see `docs/MUSIC_MODEL.md` § "The mask is a head-side
//! projection". It is now **slot-indexed** (slot 0 = the tuning root), the
//! same index space as the tick angles, so [`build_slots`] zips them by
//! index correctly.

use crate::coach::{Coach, LatestFeatures, MusicInfoRes};
use crate::state::{AppSettings, AppState, SongTonality};
use crate::ui::*;
use crate::widgets::note_dial::{DialScale, DialSlot, DialState, Needle, NeedleStyle};
use bevy::prelude::*;
use domain_ports::app_coach::{Command, FeatureSnapshot, MusicInfo};
use domain_ports::music::{tuning_view, InstrumentKey, Tonality, Tuning};
use std::f32::consts::TAU;

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

/// Walk a [`Tonality`]'s key-widths to an N-slot in-scale mask, **indexed
/// in slot space** (slot 0 = the tuning `root`). Index `i` is `true` iff
/// tuning slot `i` is one of the scale's notes.
///
/// `n` is the tuning's octave slot count (12 for 12-TET / Hindustani
/// Just, 22 for the 22-shruti grid). The mask length equals `n` and must
/// match the tick-angle table built by [`build_slots`] for the same
/// tuning — both are slot-indexed, so they zip by index.
///
/// Starts at the tonic's **slot index** — `(tonic − root) mod n`, the
/// gauge-clean delta (subtract to an interval first, never branch on an
/// absolute offset) — and adds each key-width modulo `n`, marking every
/// visited slot. The tonic itself is always lit; the final width lands
/// back on it by construction (a well-formed scale's widths sum to `n`),
/// so it isn't double-counted.
///
/// This is the **scale ring** (layer 4) — a pure integer projection of
/// the `Tonality`, tuning-independent (the lit *set* is the same in 12-TET
/// or Just; only where each tick is *drawn* differs). Scale widths and the
/// tonic are whole numbers by the `Tonality` invariant (only the live
/// slide is fractional), so rounding is exact here, not a fudge. Lives
/// head-side because the head holds the `Tonality`; the coach is not
/// consulted.
pub fn in_scale_mask(tonality: &Tonality, root: InstrumentKey, n: usize) -> Vec<bool> {
    let mut mask = vec![false; n];
    // The tonic's slot index: the gauge-clean delta tonic − root, folded
    // into [0, n). Widths are whole (Tonality invariant), so rounding is
    // exact. rem_euclid keeps a tonic below the root positive.
    let mut cursor = (tonality.tonic - root).0.round().rem_euclid(n as f32) as usize;
    mask[cursor] = true;
    for width in tonality.widths() {
        cursor = (cursor + width.0.round() as usize) % n;
        mask[cursor] = true;
    }
    mask
}

/// Build the N dial slots from a [`MusicInfo`] snapshot: each slot's tick
/// angle from [`tuning_view::slot_position_from`] (real Hz folded against
/// Sa, so a non-uniform tuning keeps its uneven spacing and Sa sits at
/// north), each slot's `active` flag from the slot-space [`in_scale_mask`].
/// N comes from the tuning — 12 for 12-TET / Hindustani Just, 22 for the
/// 22-shruti grid. Pulled out so [`spawn`] (shell) and [`repaint_slots`]
/// (paint) share one definition, and so it's unit testable without a Bevy
/// world.
fn build_slots(info: &MusicInfo) -> Vec<DialSlot> {
    let tuning = Tuning::new(info.tuning);
    let n = tuning.n();
    let mask = in_scale_mask(&info.tonality, info.tuning.root, n);
    (0..n)
        .map(|i| DialSlot {
            // Tick = slot i's within-octave position relative to Sa, ×TAU.
            angle: tuning_view::slot_position_from(&tuning, info.tonality.tonic, i) * TAU,
            label: None,
            active: mask[i],
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
pub fn spawn(mut commands: Commands) {
    let dial = commands
        .spawn((
            DespawnOnExit(AppState::InGame),
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

/// Each frame, read the latest feature snapshot and update the dial's
/// `DialState.needles`. Voiced (`f0_hz > 0`) → one primary needle at
/// [`needle_angle`] (the live pitch placed relative to Sa); unvoiced →
/// empty `needles`. Needs the [`MusicInfoRes`] snapshot for the current
/// tuning + tonic — north is Sa, so the needle is meaningless without it
/// (no snapshot yet → no needle, mirroring the empty-dial spawn state).
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
        Some(FeatureSnapshot {
            f0_hz, confidence, ..
        }),
        Some(info),
    ) = (features.0, music.0)
    else {
        // No feature snapshot or no music config yet → ensure no needle.
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

    let angle = needle_angle(&info, f0_hz);
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

/// Click the center hub → capture the live pitch as the song's root (Sa).
///
/// Resolves the live `f0` to the nearest keyboard key (octave preserved —
/// the singer's real register becomes Sa) via
/// [`tuning_view::nearest_key_of_hz`], rebuilds the [`Tonality`] keeping the
/// current scale shape, writes it to [`SongTonality`], and round-trips it
/// through the coach via `ConfigureSession`. Gated on confidence — a no-op
/// below [`CAPTURE_CONF_GATE`] (the hub is also greyed by [`sync_hub`]; this
/// is the guard).
pub fn handle_hub_capture(
    q: Query<&Interaction, (Changed<Interaction>, With<DialHub>)>,
    features: Res<LatestFeatures>,
    music: Res<MusicInfoRes>,
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
        if snap.confidence < CAPTURE_CONF_GATE || snap.f0_hz <= 0.0 {
            return; // gate: not a trustworthy pitch
        }
        // Resolve f0 → key against the *current* tuning. Prefer the read
        // model's spec (the real round-tripped tuning); fall back to the
        // head's own settings if no snapshot has landed yet.
        let spec = music
            .0
            .as_ref()
            .map(|m| m.tuning)
            .unwrap_or_else(|| settings.tuning_spec());
        let continuous = tuning_view::key_of_hz(&spec, snap.f0_hz);
        let new_tonic = tuning_view::nearest_key_of_hz(&spec, snap.f0_hz);
        info!(
            "capture-root: f0={:.1}Hz conf={:.2} → key {:.2} → snap {:.0}",
            snap.f0_hz, snap.confidence, continuous.offset, new_tonic.offset
        );
        let widths: Vec<f32> = tonality.0.widths().iter().map(|w| w.0).collect();
        tonality.0 = Tonality::new(new_tonic, &widths);
        coach.0.send_command(Command::ConfigureSession {
            tuning: settings.tuning_spec(),
            tonality: tonality.0,
        });
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
        .map(|s| s.f0_hz > 0.0 && s.confidence >= CAPTURE_CONF_GATE)
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

/// Map a live `f0` to the needle's dial angle, anchored on Sa.
///
/// The needle folds the measured **frequency** against **Sa's frequency**
/// ([`tuning_view::octave_position`]) and the dial multiplies by `TAU`.
/// This is the *same Hz fold the ticks use* ([`tuning_view::slot_position_from`]),
/// so the needle always agrees with the ticks: a perfectly-sung Just Pa
/// (3/2 above Sa) lands exactly on the uneven Just Pa tick, not on the even
/// 7/12 a key-space fold would snap it to. The needle shows where the voice
/// *actually is* in log-frequency — the intonation signal a tuning dial
/// exists to display.
///
/// No frequency is hardcoded: Sa's Hz comes from resolving the tonic
/// through the snapshot's tuning ([`tuning_view::hz`]). Result is in
/// `[0, TAU)`, clock convention: 0 = 12 o'clock = Sa, clockwise. Sa itself
/// lands at 0; callers gate on `f0_hz > 0` before reaching here.
fn needle_angle(info: &MusicInfo, f0_hz: f32) -> f32 {
    let sa_hz = tuning_view::hz(&Tuning::new(info.tuning), info.tonality.tonic);
    tuning_view::octave_position(f0_hz, sa_hz) * TAU
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::music::{harmonium_key, tuning_view, TuningKind, TuningSpec};

    const BILAWAL: [f32; 7] = [2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0];

    /// A=440 12-TET, tuning root at A (key 21) — the head's default tuning.
    fn tet_spec() -> TuningSpec {
        TuningSpec {
            root_note_hz: 440.0,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(21.0),
        }
    }

    fn just_spec() -> TuningSpec {
        TuningSpec {
            root_note_hz: 440.0,
            kind: TuningKind::HindustaniJust,
            root: harmonium_key(21.0),
        }
    }

    fn info(spec: TuningSpec, tonic: f32, widths: &[f32]) -> MusicInfo {
        MusicInfo {
            tuning: spec,
            tonality: Tonality::new(harmonium_key(tonic), widths),
        }
    }

    // --- in_scale_mask (slot-space scale-ring projection) -------------

    #[test]
    fn mask_lights_seven_when_tonic_is_the_tuning_root() {
        // Tonic ON the tuning root (key 21 = A): slot index (21−21)=0, so
        // the mask reads slot-space identical to the old key-space-at-0
        // case: Sa Re Ga Ma Pa Dha Ni at slots 0,2,4,5,7,9,11.
        let t = Tonality::new(harmonium_key(21.0), &BILAWAL);
        let mask = in_scale_mask(&t, harmonium_key(21.0), 12);
        let expected = vec![
            true, false, true, false, true, true, false, true, false, true, false, true,
        ];
        assert_eq!(mask, expected);
        assert_eq!(mask.iter().filter(|b| **b).count(), 7);
    }

    #[test]
    fn mask_is_slot_space_relative_to_the_tuning_root() {
        // Bilawal, Sa on D (key 14), tuning root A (key 21). Sa's slot is
        // (14−21).rem_euclid(12) = 5. Walk widths from 5:
        //   5 →2→ 7 →2→ 9 →1→ 10 →2→ 0 →2→ 2 →2→ 4 →1→ 5(close)
        // Lit slots: {0,2,4,5,7,9,10}.
        let mask = in_scale_mask(
            &Tonality::new(harmonium_key(14.0), &BILAWAL),
            harmonium_key(21.0),
            12,
        );
        let lit: Vec<usize> = mask
            .iter()
            .enumerate()
            .filter(|(_, &b)| b)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(lit, vec![0, 2, 4, 5, 7, 9, 10]);
    }

    #[test]
    fn mask_is_gauge_invariant() {
        // Shift tonic AND root by the same constant → identical mask (the
        // walk depends only on the delta tonic − root, not absolute keys).
        let a = in_scale_mask(
            &Tonality::new(harmonium_key(14.0), &BILAWAL),
            harmonium_key(21.0),
            12,
        );
        let b = in_scale_mask(
            &Tonality::new(harmonium_key(14.0 + 100.0), &BILAWAL),
            harmonium_key(21.0 + 100.0),
            12,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn mask_folds_tonic_above_one_octave() {
        // A tonic an octave up folds to the same mask (the ring shows one
        // octave): both reduce to the same slot index mod 12.
        let high = in_scale_mask(
            &Tonality::new(harmonium_key(26.0), &BILAWAL),
            harmonium_key(21.0),
            12,
        );
        let low = in_scale_mask(
            &Tonality::new(harmonium_key(14.0), &BILAWAL),
            harmonium_key(21.0),
            12,
        );
        assert_eq!(high, low);
    }

    #[test]
    fn twenty_two_shruti_mask_has_22_slots_and_7_lit() {
        // Bilawal on the 22-shruti grid, tonic on the tuning root (slot 0).
        // Widths [3,2,4,4,3,2,4] walk to slots 0,3,5,9,13,16,18.
        let t = Tonality::new(harmonium_key(21.0), &[3.0, 2.0, 4.0, 4.0, 3.0, 2.0, 4.0]);
        let mask = in_scale_mask(&t, harmonium_key(21.0), 22);
        assert_eq!(mask.len(), 22);
        let lit: Vec<usize> = mask
            .iter()
            .enumerate()
            .filter(|(_, &b)| b)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(lit, vec![0, 3, 5, 9, 13, 16, 18]);
    }

    // --- build_slots: Sa at north, ticks slot-indexed, mask zipped -----

    #[test]
    fn build_slots_puts_sa_at_north() {
        // Bilawal on D, 12-TET. Sa's slot (slot 5, = key 14) must sit at
        // angle 0 (north) and be lit; the tuning root A (slot 0) rotates to
        // the 7-o'clock Pa position (7/12 · TAU).
        let slots = build_slots(&info(tet_spec(), 14.0, &BILAWAL));
        assert_eq!(slots.len(), 12);
        let north = slots[5].angle;
        assert!(
            north.abs() < 1e-4 || (north - TAU).abs() < 1e-4,
            "Sa at north: {north}"
        );
        assert!(slots[5].active, "Sa slot lit");
        assert!((slots[0].angle - 7.0 * TAU / 12.0).abs() < 1e-4, "root→Pa");
        assert!(slots[0].active, "Pa slot lit");
    }

    #[test]
    fn build_slots_active_matches_the_mask() {
        let info = info(tet_spec(), 14.0, &BILAWAL);
        let slots = build_slots(&info);
        let mask = in_scale_mask(&info.tonality, info.tuning.root, 12);
        for (i, slot) in slots.iter().enumerate() {
            assert_eq!(slot.active, mask[i], "slot {i} active");
            assert!(slot.label.is_none(), "slot {i} label — head is vocab-free");
        }
    }

    #[test]
    fn build_slots_just_keeps_uneven_ticks() {
        // Just intonation moves tick *angles* (Pa at true 3/2) while the
        // lit set — a pure tonality projection — matches 12-TET's.
        let tonic = 14.0;
        let tet = build_slots(&info(tet_spec(), tonic, &BILAWAL));
        let just = build_slots(&info(just_spec(), tonic, &BILAWAL));
        // Pa slot (slot 0, the root A) angle differs between tunings.
        // In 12-TET it's exactly 7/12; in Just it's log2(3/2) past Sa.
        assert!(
            (tet[0].angle - just[0].angle).abs() > 1e-4,
            "Just must move a tick angle off the even grid"
        );
        for i in 0..12 {
            assert_eq!(tet[i].active, just[i].active, "slot {i} active set");
        }
    }

    // --- needle_angle: live pitch placed relative to Sa ----------------

    #[test]
    fn needle_sa_lands_at_north() {
        // Sing exactly Sa (D's frequency) → needle at 0 (north).
        let info = info(tet_spec(), 14.0, &BILAWAL);
        let sa_hz = tuning_view::hz(&Tuning::new(info.tuning), harmonium_key(14.0));
        let a = needle_angle(&info, sa_hz);
        assert!(a.abs() < 1e-3 || (a - TAU).abs() < 1e-3, "got {a}");
    }

    #[test]
    fn needle_lands_on_each_swara_tick() {
        // The core invariant: every Bilawal swara's needle sits exactly on
        // its lit tick — in BOTH tunings (proves it's not a 12-TET fluke).
        for spec in [tet_spec(), just_spec()] {
            let info = info(spec, 14.0, &BILAWAL);
            let tuning = Tuning::new(info.tuning);
            let slots = build_slots(&info);
            // Walk the scale's keys; each must produce a needle equal to the
            // angle of the lit slot it lands on.
            let mut key = info.tonality.tonic;
            let mut widths = info.tonality.widths().to_vec();
            widths.pop(); // drop the closing octave width
            let mut degree_keys = vec![key];
            for w in widths {
                key = key + w;
                degree_keys.push(key);
            }
            for k in degree_keys {
                let hz = tuning_view::hz(&tuning, k);
                let needle = needle_angle(&info, hz);
                // Which slot is this key? (k − root) mod 12.
                let slot = (k - info.tuning.root).0.round().rem_euclid(12.0) as usize;
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
        // real Hz, not a 12-TET assumption.
        let info = info(just_spec(), 14.0, &BILAWAL);
        let tuning = Tuning::new(info.tuning);
        let sa_hz = tuning_view::hz(&tuning, harmonium_key(14.0));
        let needle = needle_angle(&info, sa_hz * 1.5);
        let just_pa = (1.5_f32).log2().rem_euclid(1.0) * TAU;
        assert!(
            (needle - just_pa).abs() < 1e-3,
            "Just Pa needle {needle} vs {just_pa}"
        );
    }
}
