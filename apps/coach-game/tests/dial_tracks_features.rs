//! Integration tests for `game::note_dial::update_from_features`: voiced
//! → primary needle at the expected angle; unvoiced or no snapshot →
//! no needle; transitioning from voiced to unvoiced clears the needle.
//!
//! Drives the production schedule with a `FakeCoach` whose
//! `latest_features` slot is set per-test.

mod common;

use bevy::prelude::*;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::widgets::note_dial::{DialState, NoteDialRoot};
use common::{build_test_app, pump};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::pitch::PitchLog2;
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind, ORIGIN};

/// The test's musical frame: A=440 12-TET, Bilawal with Sa on D (slot 5,
/// five semitones above the A=440 reference). North of the dial is Sa, so
/// needle angles are measured from D.
fn test_music() -> MusicInfo {
    let rotation = (PitchLog2::from_hz(440.0) - ORIGIN).fract();
    let tuning = TuningAbsolute::new(TuningKind::TwelveTet.intervals(), rotation);
    let intervals = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
    // D ≈ 294 Hz sits on helix floor 8 (floor(log2 294)).
    MusicInfo {
        scale: Scale::new(intervals, tuning.shift_up(5), 8),
    }
}

/// Hz of a scale degree in the test frame — used to feed the needle a
/// pitch we know the target angle for.
fn hz_of_degree(degree: usize) -> f32 {
    test_music().scale.pitch_at(degree).to_hz()
}

/// Drive into InGame so the dial entity exists, with the musical frame
/// published into `MusicInfoRes` (the needle needs Sa to measure from).
fn enter_in_game(app: &mut App, fake: &common::FakeCoach) {
    // Initial OnEnter(MainMenu).
    pump(app);
    // Publish the musical frame: set the snapshot the head reads, and emit
    // the event that makes `drain_events` refresh `MusicInfoRes` from it.
    {
        let mut g = fake.inner.lock().unwrap();
        let m = test_music();
        g.music_info = Some(m);
        g.pending_events
            .push(CoachEvent::MusicSessionConfigured { scale: m.scale });
    }
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
}

fn set_features(fake: &common::FakeCoach, snap: Option<FeatureSnapshot>) {
    fake.inner.lock().unwrap().latest_features = snap;
}

fn dial_needles(app: &mut App) -> Vec<f32> {
    app.world_mut()
        .query_filtered::<&DialState, With<NoteDialRoot>>()
        .iter(app.world())
        .flat_map(|s| s.needles.iter().map(|n| n.angle))
        .collect()
}

#[test]
fn voiced_feature_writes_a_primary_needle_at_expected_angle() {
    use std::f32::consts::TAU;
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app, &fake);

    // Sing A=440 (key 21, the tuning root). Relative to Sa on D (key 14),
    // A is 7 semitones up → Pa → needle at 7/12 of the circle.
    set_features(
        &fake,
        Some(FeatureSnapshot {
            hop_index: 0,
            f0_hz: 440.0,
            confidence: 0.9,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms: 1,
        }),
    );
    app.update();

    let angles = dial_needles(&mut app);
    assert_eq!(angles.len(), 1, "voiced f0 should yield exactly one needle");
    let expected = 7.0 * TAU / 12.0; // A is Pa (7 semitones) above Sa=D
    assert!(
        (angles[0] - expected).abs() < 1e-3,
        "needle angle {} != expected {} (A should be Pa above Sa=D)",
        angles[0],
        expected
    );
}

#[test]
fn singing_sa_lands_the_needle_at_north() {
    use std::f32::consts::TAU;
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app, &fake);

    // Sing exactly Sa (D, degree 0) → needle at north (0).
    set_features(
        &fake,
        Some(FeatureSnapshot {
            hop_index: 0,
            f0_hz: hz_of_degree(0),
            confidence: 0.9,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms: 1,
        }),
    );
    app.update();

    let angles = dial_needles(&mut app);
    assert_eq!(angles.len(), 1);
    assert!(
        angles[0].abs() < 1e-3 || (angles[0] - TAU).abs() < 1e-3,
        "Sa should sit at north, got {}",
        angles[0]
    );
}

#[test]
fn unvoiced_feature_yields_no_needle() {
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app, &fake);

    set_features(
        &fake,
        Some(FeatureSnapshot {
            hop_index: 0,
            f0_hz: 0.0, // unvoiced
            confidence: 0.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms: 1,
        }),
    );
    app.update();

    assert!(
        dial_needles(&mut app).is_empty(),
        "unvoiced snapshot should produce zero needles"
    );
}

#[test]
fn voiced_then_unvoiced_clears_the_needle() {
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app, &fake);

    // Frame 1: voiced.
    set_features(
        &fake,
        Some(FeatureSnapshot {
            hop_index: 0,
            f0_hz: 261.625_56, // C4
            confidence: 0.9,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms: 1,
        }),
    );
    app.update();
    assert_eq!(dial_needles(&mut app).len(), 1);

    // Frame 2: singer stops; same feature shape but f0 = 0.
    set_features(
        &fake,
        Some(FeatureSnapshot {
            hop_index: 1,
            f0_hz: 0.0,
            confidence: 0.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms: 2,
        }),
    );
    app.update();

    assert!(
        dial_needles(&mut app).is_empty(),
        "needle should be cleared once f0 returns to 0"
    );
}

#[test]
fn no_snapshot_means_no_needle() {
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app, &fake);

    // FakeCoach's latest_features defaults to None — leave it that way.
    set_features(&fake, None);
    app.update();

    assert!(
        dial_needles(&mut app).is_empty(),
        "no FeatureSnapshot yet should produce zero needles"
    );
}
