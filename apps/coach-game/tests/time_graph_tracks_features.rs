//! Headless integration coverage for the time graph tree and scene application.

mod common;

use bevy::prelude::*;
use coach_game::game::InGameRoot;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::widgets::time_graph::{
    BreathSpanMarker, GrooveLineMarker, OnsetTickMarker, TimeGraphEventsLane,
    TimeGraphGridSceneRes, TimeGraphLiveSceneRes, TimeGraphPitchLane, TimeGraphRoot,
};
use common::{build_test_app, pump};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::pitch::{PitchLog2, PitchLog2Interval};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

fn publish_music(fake: &common::FakeCoach, info: MusicInfo) {
    let mut state = fake.inner.lock().unwrap();
    state.music_info = Some(info);
    state
        .pending_events
        .push(CoachEvent::SessionConfigured { scale: info.scale });
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

fn enter_in_game(app: &mut App) {
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
    assert_eq!(
        *app.world()
            .resource::<State<coach_game::state::AppState>>()
            .get(),
        coach_game::state::AppState::InGame
    );
}

#[test]
fn semantic_graph_projects_into_tree_and_lane_nodes() {
    let (mut app, fake) = build_test_app();
    pump(&mut app);
    enter_in_game(&mut app);

    publish_music(&fake, music(4));
    fake.inner.lock().unwrap().pending_features = vec![
        snapshot(0, 1_000, Some(PitchLog2(8.0)), 0.7, 0.0),
        snapshot(1, 1_010, Some(PitchLog2(8.1)), 0.0, 1.0),
        snapshot(2, 1_020, Some(PitchLog2(8.2)), 0.0, 1.0),
        snapshot(3, 1_030, Some(PitchLog2(8.3)), 0.4, 0.0),
    ];
    app.update();

    let grid = app.world().resource::<TimeGraphGridSceneRes>().clone();
    let live = app.world().resource::<TimeGraphLiveSceneRes>().clone();
    assert!(!grid.grooves.is_empty());
    assert_eq!(live.onset_ticks.len(), 2);
    assert_eq!(live.breath_spans.len(), 1);

    let world = app.world_mut();
    let roots = {
        world
            .query_filtered::<Entity, With<InGameRoot>>()
            .iter(world)
            .collect::<Vec<_>>()
    };
    let graph_roots = {
        world
            .query_filtered::<(&ChildOf, Entity), With<TimeGraphRoot>>()
            .iter(world)
            .map(|(parent, entity)| (parent.parent(), entity))
            .collect::<Vec<_>>()
    };
    let pitch_lane = {
        world
            .query_filtered::<(&ChildOf, Entity), With<TimeGraphPitchLane>>()
            .iter(world)
            .map(|(parent, entity)| (parent.parent(), entity))
            .collect::<Vec<_>>()
    };
    let events_lane = {
        world
            .query_filtered::<(&ChildOf, Entity), With<TimeGraphEventsLane>>()
            .iter(world)
            .map(|(parent, entity)| (parent.parent(), entity))
            .collect::<Vec<_>>()
    };

    assert_eq!(roots.len(), 1);
    assert_eq!(graph_roots.len(), 1);
    assert_eq!(pitch_lane.len(), 1);
    assert_eq!(events_lane.len(), 1);
    assert_eq!(graph_roots[0].0, roots[0]);
    assert_eq!(pitch_lane[0].0, graph_roots[0].1);
    assert_eq!(events_lane[0].0, graph_roots[0].1);

    let grooves = world
        .query_filtered::<(&GrooveLineMarker, &Node), With<GrooveLineMarker>>()
        .iter(world)
        .collect::<Vec<_>>();
    assert_eq!(grooves.len(), grid.grooves.len());
    let groove_y = grid.grooves[0].y;
    assert!(grooves.iter().any(
        |(_, node)| matches!(node.top, Val::Percent(v) if (v - (1.0 - groove_y) * 100.0).abs() < 1e-5)
    ));

    let ticks = world
        .query_filtered::<(&OnsetTickMarker, &Node), With<OnsetTickMarker>>()
        .iter(world)
        .collect::<Vec<_>>();
    assert_eq!(ticks.len(), live.onset_ticks.len());

    let spans = world
        .query_filtered::<(&BreathSpanMarker, &Node), With<BreathSpanMarker>>()
        .iter(world)
        .collect::<Vec<_>>();
    assert_eq!(spans.len(), live.breath_spans.len());
}
