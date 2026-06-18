//! Headless integration coverage for ordered feature history and the
//! semantic time-graph model.

mod common;

use bevy::prelude::*;
use coach_game::coach::FeatureHistoryRes;
use coach_game::game::SemanticGraphRes;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::state::AppState;
use common::{build_test_app, pump};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::pitch::{PitchLog2, PitchLog2Interval};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

fn music(octave: i32) -> MusicInfo {
    let tuning = TuningAbsolute::new(TuningKind::TwelveTet.intervals(), PitchLog2Interval(0.17));
    MusicInfo {
        scale: Scale::new(
            ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
            tuning.shift_up(3),
            octave,
        ),
    }
}

fn snapshot(
    hop_index: u64,
    t_ms: u64,
    pitch: Option<PitchLog2>,
    onset: f32,
    breath: f32,
) -> FeatureSnapshot {
    FeatureSnapshot {
        hop_index,
        f0_hz: pitch.map_or(0.0, PitchLog2::to_hz),
        confidence: 0.8,
        onset,
        breath,
        vibrato_rate: 5.5,
        vibrato_depth: 0.2,
        t_ms,
    }
}

fn publish_music(fake: &common::FakeCoach, info: MusicInfo) {
    let mut state = fake.inner.lock().unwrap();
    state.music_info = Some(info);
    state
        .pending_events
        .push(CoachEvent::MusicSessionConfigured { scale: info.scale });
}

fn enter_in_game(app: &mut App) {
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
    assert_eq!(
        *app.world().resource::<State<AppState>>().get(),
        AppState::InGame
    );
}

#[test]
fn drained_hops_feed_semantic_graph_and_ignore_scale_register() {
    let (mut app, fake) = build_test_app();
    pump(&mut app);
    publish_music(&fake, music(4));
    enter_in_game(&mut app);

    {
        let mut state = fake.inner.lock().unwrap();
        state.pending_features = vec![
            snapshot(0, 1_000, Some(PitchLog2(8.0)), 0.7, 0.0),
            snapshot(1, 1_010, Some(PitchLog2(8.1)), 0.0, 1.0),
            snapshot(2, 1_020, Some(PitchLog2(8.2)), 0.0, 1.0),
            snapshot(3, 1_030, Some(PitchLog2(8.3)), 0.4, 0.0),
        ];
    }
    app.update();

    let graph = &app.world().resource::<SemanticGraphRes>().0;
    assert_eq!(graph.trace_segments.len(), 1);
    assert_eq!(graph.trace_segments[0].points.len(), 4);
    assert_eq!(
        graph
            .onset_ticks
            .iter()
            .map(|tick| tick.t_ms)
            .collect::<Vec<_>>(),
        vec![1_000, 1_030]
    );
    assert_eq!(graph.breath_spans[0].start_ms, 1_010);
    assert_eq!(graph.breath_spans[0].end_ms, 1_020);
    assert!(!graph.grooves.is_empty());
    let groove_pitches = graph
        .grooves
        .iter()
        .map(|groove| groove.pitch)
        .collect::<Vec<_>>();

    publish_music(&fake, music(11));
    app.update();
    let shifted_register_pitches = app
        .world()
        .resource::<SemanticGraphRes>()
        .0
        .grooves
        .iter()
        .map(|groove| groove.pitch)
        .collect::<Vec<_>>();
    assert_eq!(
        shifted_register_pitches, groove_pitches,
        "scale register must not position the octave-repeating tuning grid"
    );
}

#[test]
fn history_is_discarded_outside_in_game_and_cleared_on_exit() {
    let (mut app, fake) = build_test_app();
    pump(&mut app);

    fake.inner.lock().unwrap().pending_features =
        vec![snapshot(0, 10, Some(PitchLog2(8.0)), 0.0, 0.0)];
    app.update();
    assert!(
        app.world().resource::<FeatureHistoryRes>().0.is_empty(),
        "always-on draining must discard history while outside InGame"
    );

    publish_music(&fake, music(8));
    enter_in_game(&mut app);
    fake.inner.lock().unwrap().pending_features =
        vec![snapshot(0, 20, Some(PitchLog2(8.0)), 0.0, 0.0)];
    app.update();
    assert_eq!(app.world().resource::<FeatureHistoryRes>().0.len(), 1);

    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::MainMenu);
    pump(&mut app);
    assert!(app.world().resource::<FeatureHistoryRes>().0.is_empty());
    assert_eq!(
        app.world().resource::<SemanticGraphRes>().0,
        Default::default()
    );
}
