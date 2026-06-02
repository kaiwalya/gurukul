//! Integration tests for `game::dial::update_from_features`: voiced
//! → primary needle at the expected angle; unvoiced or no snapshot →
//! no needle; transitioning from voiced to unvoiced clears the needle.
//!
//! Drives the production schedule with a `FakeCoach` whose
//! `latest_features` slot is set per-test.

mod common;

use bevy::prelude::*;
use coach_game::game::dial::{angle_from_f0, InGameDial};
use coach_game::menu::main_menu::NewGameButton;
use coach_game::widgets::note_dial::DialState;
use common::{build_test_app, pump};
use domain_ports::app_coach::FeatureSnapshot;

/// Drive into InGame so the dial entity exists.
fn enter_in_game(app: &mut App) {
    // Initial OnEnter(MainMenu).
    pump(app);
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
}

fn set_features(fake: &common::FakeCoach, snap: Option<FeatureSnapshot>) {
    fake.inner.lock().unwrap().latest_features = snap;
}

fn dial_needles(app: &mut App) -> Vec<f32> {
    app.world_mut()
        .query_filtered::<&DialState, With<InGameDial>>()
        .iter(app.world())
        .flat_map(|s| s.needles.iter().map(|n| n.angle))
        .collect()
}

#[test]
fn voiced_feature_writes_a_primary_needle_at_expected_angle() {
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app);

    set_features(
        &fake,
        Some(FeatureSnapshot {
            f0_hz: 440.0, // A4: 9 semitones above C
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
    let expected = angle_from_f0(440.0);
    assert!(
        (angles[0] - expected).abs() < 1e-4,
        "needle angle {} != expected {}",
        angles[0],
        expected
    );
}

#[test]
fn unvoiced_feature_yields_no_needle() {
    let (mut app, fake) = build_test_app();
    enter_in_game(&mut app);

    set_features(
        &fake,
        Some(FeatureSnapshot {
            f0_hz: 0.0, // unvoiced
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
    enter_in_game(&mut app);

    // Frame 1: voiced.
    set_features(
        &fake,
        Some(FeatureSnapshot {
            f0_hz: 261.625_56, // C4
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
            f0_hz: 0.0,
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
    enter_in_game(&mut app);

    // FakeCoach's latest_features defaults to None — leave it that way.
    set_features(&fake, None);
    app.update();

    assert!(
        dial_needles(&mut app).is_empty(),
        "no FeatureSnapshot yet should produce zero needles"
    );
}
