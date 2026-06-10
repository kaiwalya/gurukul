//! Layer-3 (layout-aware) coverage for the time graph.
//!
//! The *only* level that runs the real Bevy UI layout schedule, so the
//! only one that can see the measure→paint seam: `ComputedNode` (measured
//! physical sizes) and `UiGlobalTransform` (global screen positions). It
//! runs at **scale factor 2.0** on purpose — at 1× the physical and
//! logical pixel frames coincide, so a frame-mismatch bug is invisible and
//! every assertion here would pass against the broken code. See
//! `ARCHITECTURE.md` ("Testability follows the layers") and the layer-3
//! rules in `CONTRIBUTING.md`.
//!
//! The trace is produced by the **real** pipeline (canned features →
//! semantic graph → scene → measured-size capture → paint); nothing is
//! injected. The headline assertion is global containment: every painted
//! trace body's global rect lies inside the pitch lane's global rect —
//! "is it on screen where it should be", the question levels 1 and 2
//! cannot ask.

mod common;

use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};
use coach_game::menu::main_menu::NewGameButton;
use coach_game::widgets::time_graph::{TimeGraphPitchLane, TraceSegmentBody};
use common::{build_layout_test_app, pump, pump_layout};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::pitch::{PitchLog2, PitchLog2Interval};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

/// An axis-aligned rect in physical pixels, derived from a UI node's
/// global transform (center) and its computed size (extent).
#[derive(Debug, Clone, Copy)]
struct GlobalRect {
    min: Vec2,
    max: Vec2,
}

impl GlobalRect {
    /// The node's true global axis-aligned bounds, accounting for any
    /// `UiTransform` rotation. `ComputedNode::size()` is the *unrotated*
    /// layout box, so for a rotated body (the trace segments) the naive
    /// `center ± size/2` is wrong; we transform the four local corners
    /// through the node's affine and take their bounding box.
    fn from_node(xform: &UiGlobalTransform, node: &ComputedNode) -> Self {
        let half = node.size() * 0.5;
        let affine = xform.affine();
        let corners = [
            Vec2::new(-half.x, -half.y),
            Vec2::new(half.x, -half.y),
            Vec2::new(half.x, half.y),
            Vec2::new(-half.x, half.y),
        ]
        .map(|c| affine.transform_point2(c));
        let min = corners.iter().copied().reduce(Vec2::min).unwrap();
        let max = corners.iter().copied().reduce(Vec2::max).unwrap();
        Self { min, max }
    }

    /// True when `self` lies fully within `outer`, allowing a small
    /// sub-pixel tolerance for rounding at the edges.
    fn within(&self, outer: &GlobalRect) -> bool {
        const EPS: f32 = 1.0;
        self.min.x >= outer.min.x - EPS
            && self.min.y >= outer.min.y - EPS
            && self.max.x <= outer.max.x + EPS
            && self.max.y <= outer.max.y + EPS
    }
}

fn publish_music(fake: &common::FakeCoach, info: MusicInfo) {
    let mut state = fake.inner.lock().unwrap();
    state.music_info = Some(info);
    state
        .pending_events
        .push(CoachEvent::SessionConfigured { scale: info.scale });
}

fn snapshot(hop_index: u64, t_ms: u64, pitch: PitchLog2) -> FeatureSnapshot {
    FeatureSnapshot {
        hop_index,
        f0_hz: pitch.to_hz(),
        confidence: 0.8,
        onset: 0.0,
        breath: 0.0,
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
        coach_game::state::AppState::InGame,
        "NewGame must transition into InGame"
    );
}

/// Feed a rising pitch run so the projection emits a multi-point pitch
/// trace, then drive the layout+capture+paint loop to settle.
fn drive_trace(app: &mut App, fake: &common::FakeCoach) {
    publish_music(fake, music(4));
    fake.inner.lock().unwrap().pending_features = vec![
        snapshot(0, 1_000, PitchLog2(8.0)),
        snapshot(1, 1_010, PitchLog2(8.1)),
        snapshot(2, 1_020, PitchLog2(8.2)),
        snapshot(3, 1_030, PitchLog2(8.3)),
    ];
    pump_layout(app);
}

fn pitch_lane_rect(world: &mut World) -> GlobalRect {
    let (xform, node) = world
        .query_filtered::<(&UiGlobalTransform, &ComputedNode), With<TimeGraphPitchLane>>()
        .single(world)
        .expect("pitch lane has a global transform and computed node");
    GlobalRect::from_node(xform, node)
}

fn trace_body_rects(world: &mut World) -> Vec<GlobalRect> {
    world
        .query_filtered::<(&UiGlobalTransform, &ComputedNode), With<TraceSegmentBody>>()
        .iter(world)
        .map(|(xform, node)| GlobalRect::from_node(xform, node))
        .collect()
}

#[test]
fn trace_bodies_are_painted_inside_the_pitch_lane_at_2x() {
    let (mut app, fake) = build_layout_test_app();
    pump(&mut app);
    enter_in_game(&mut app);
    drive_trace(&mut app, &fake);

    let world = app.world_mut();

    // The lane was actually laid out: a real, non-degenerate measured size.
    let lane = pitch_lane_rect(world);
    let lane_size = lane.max - lane.min;
    assert!(
        lane_size.x > 0.0 && lane_size.y > 0.0,
        "pitch lane must have a non-degenerate measured size, got {lane_size:?}"
    );

    // Existence: the real paint system actually ran and produced bodies.
    // (Catches the worst failure — a paint pass that never runs in the suite.)
    let bodies = trace_body_rects(world);
    assert!(
        !bodies.is_empty(),
        "the real pipeline must paint at least one trace body for a multi-point pitch run"
    );

    // Placement: every painted body lies inside the lane it belongs to.
    // This is the assertion the screenshot bug fails — at 2× the raw
    // physical-pixel size feeds logical `px()` positions, doubling every
    // coordinate so bodies escape the lane.
    for (i, body) in bodies.iter().enumerate() {
        assert!(
            body.within(&lane),
            "trace body {i} escapes the pitch lane:\n  body = {body:?}\n  lane = {lane:?}"
        );
    }
}
