//! Time-graph widget: normalized scene in, UI nodes out.
//!
//! This module owns only panel geometry and lane-local normalized
//! coordinates. Musical semantics stay upstream in `graph_model`.

use crate::graph_model::{
    BreathSpan, GrooveLine, OnsetTick, PitchWindow, SemanticGraph, TimeWindow, TraceSegment,
};
use bevy::prelude::*;
use bevy::ui::ComputedNode;
use domain_ports::pitch::PitchLog2;

const ROOT_LEFT: f32 = 32.0;
const ROOT_TOP: f32 = 96.0;
const ROOT_RIGHT: f32 = 376.0;
const ROOT_BOTTOM: f32 = 24.0;
const LANE_GAP: f32 = 12.0;
const LANE_PADDING: f32 = 10.0;
const GROOVE_HEIGHT: f32 = 2.0;
const TICK_WIDTH: f32 = 2.0;
const TRACE_WIDTH: f32 = 3.0;

const COLOR_ROOT: Color = Color::srgba(0.08, 0.08, 0.11, 0.94);
const COLOR_PITCH_LANE: Color = Color::srgba(0.10, 0.10, 0.13, 0.96);
const COLOR_EVENT_LANE: Color = Color::srgba(0.11, 0.11, 0.14, 0.94);
const COLOR_GROOVE_ACTIVE: Color = Color::srgb(0.45, 0.70, 0.95);
const COLOR_GROOVE_INACTIVE: Color = Color::srgba(0.56, 0.56, 0.63, 0.26);
const COLOR_ONSET: Color = Color::srgb(1.0, 0.86, 0.44);
const COLOR_BREATH: Color = Color::srgba(0.88, 0.64, 0.95, 0.34);
const COLOR_TRACE: Color = Color::srgb(0.94, 0.94, 0.98);
const TRACE_MIN_ALPHA: f32 = 0.25;
const TRACE_MAX_ALPHA: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedTracePoint {
    pub point: NormalizedPoint,
    pub confidence: f32,
    pub vibrato_rate: f32,
    pub vibrato_depth: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedTraceSegment {
    pub points: Vec<NormalizedTracePoint>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedGrooveLine {
    pub y: f32,
    pub slot: usize,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedOnsetTick {
    pub x: f32,
    pub strength: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedBreathSpan {
    pub x0: f32,
    pub x1: f32,
    pub peak: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeGraphScene {
    pub pitch_segments: Vec<NormalizedTraceSegment>,
    pub grooves: Vec<NormalizedGrooveLine>,
    pub onset_ticks: Vec<NormalizedOnsetTick>,
    pub breath_spans: Vec<NormalizedBreathSpan>,
}

#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct TimeGraphSceneRes(pub TimeGraphScene);

#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct TimeGraphPitchLaneSize(pub Option<Vec2>);

#[derive(Component)]
pub struct TimeGraphRoot;

#[derive(Component)]
pub struct TimeGraphPitchLane;

#[derive(Component)]
pub struct TimeGraphEventsLane;

#[derive(Component)]
pub struct GrooveLineMarker;

#[derive(Component)]
pub struct OnsetTickMarker;

#[derive(Component)]
pub struct BreathSpanMarker;

#[derive(Component)]
pub struct TraceSegmentBody;

#[derive(Debug, Clone, Copy, PartialEq)]
struct TraceSegmentGeom {
    center: Vec2,
    length: f32,
    angle: f32,
}

pub fn spawn(mut commands: Commands, parent: Entity) {
    let root = commands
        .spawn((
            ChildOf(parent),
            TimeGraphRoot,
            Node {
                position_type: PositionType::Absolute,
                left: px(ROOT_LEFT),
                top: px(ROOT_TOP),
                right: px(ROOT_RIGHT),
                bottom: px(ROOT_BOTTOM),
                flex_direction: FlexDirection::Column,
                row_gap: px(LANE_GAP),
                padding: UiRect::all(px(LANE_PADDING)),
                ..default()
            },
            BackgroundColor(COLOR_ROOT),
        ))
        .id();

    commands.entity(root).with_children(|parent| {
        parent.spawn((
            TimeGraphPitchLane,
            Node {
                position_type: PositionType::Relative,
                flex_grow: 4.0,
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(COLOR_PITCH_LANE),
        ));
        parent.spawn((
            TimeGraphEventsLane,
            Node {
                position_type: PositionType::Relative,
                flex_grow: 1.0,
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(COLOR_EVENT_LANE),
        ));
    });
}

pub fn apply_scene(
    mut commands: Commands,
    scene: Res<TimeGraphSceneRes>,
    pitch_lane: Query<(Entity, Option<&Children>), With<TimeGraphPitchLane>>,
    events_lane: Query<(Entity, Option<&Children>), With<TimeGraphEventsLane>>,
) {
    let pitch_children_empty = pitch_lane
        .single()
        .ok()
        .and_then(|(_, children)| children.map(|children| children.is_empty()))
        .unwrap_or(true);
    let event_children_empty = events_lane
        .single()
        .ok()
        .and_then(|(_, children)| children.map(|children| children.is_empty()))
        .unwrap_or(true);
    if !scene.is_changed() && !pitch_children_empty && !event_children_empty {
        return;
    }

    let Ok((pitch_entity, _)) = pitch_lane.single() else {
        return;
    };
    let Ok((events_entity, _)) = events_lane.single() else {
        return;
    };

    commands.entity(pitch_entity).despawn_related::<Children>();
    commands.entity(events_entity).despawn_related::<Children>();

    for groove in &scene.0.grooves {
        commands.entity(pitch_entity).with_child((
            GrooveLineMarker,
            Node {
                position_type: PositionType::Absolute,
                left: percent(0.0),
                top: percent((1.0 - groove.y).clamp(0.0, 1.0) * 100.0),
                width: percent(100.0),
                height: px(GROOVE_HEIGHT),
                ..default()
            },
            BackgroundColor(if groove.active {
                COLOR_GROOVE_ACTIVE
            } else {
                COLOR_GROOVE_INACTIVE
            }),
        ));
    }

    for onset in &scene.0.onset_ticks {
        commands.entity(events_entity).with_child((
            OnsetTickMarker,
            Node {
                position_type: PositionType::Absolute,
                left: percent(onset.x.clamp(0.0, 1.0) * 100.0),
                top: percent(0.0),
                width: px(TICK_WIDTH),
                height: percent(100.0),
                ..default()
            },
            BackgroundColor(COLOR_ONSET.with_alpha((0.35 + onset.strength * 0.65).clamp(0.0, 1.0))),
        ));
    }

    for span in &scene.0.breath_spans {
        commands.entity(events_entity).with_child((
            BreathSpanMarker,
            Node {
                position_type: PositionType::Absolute,
                left: percent(span.x0.clamp(0.0, 1.0) * 100.0),
                top: percent(0.0),
                width: percent(((span.x1 - span.x0).clamp(0.0, 1.0)) * 100.0),
                height: percent(100.0),
                ..default()
            },
            BackgroundColor(COLOR_BREATH.with_alpha((0.15 + span.peak * 0.35).clamp(0.0, 1.0))),
        ));
    }
}

pub fn apply_trace_scene(
    mut commands: Commands,
    scene: Res<TimeGraphSceneRes>,
    pitch_lane: Query<Entity, With<TimeGraphPitchLane>>,
    lane_size: Res<TimeGraphPitchLaneSize>,
    existing_bodies: Query<(Entity, &ChildOf), With<TraceSegmentBody>>,
) {
    let Ok(pitch_entity) = pitch_lane.single() else {
        return;
    };
    let has_existing = existing_bodies
        .iter()
        .any(|(_, parent)| parent.parent() == pitch_entity);
    if !scene.is_changed() && has_existing {
        return;
    }
    let Some(size) = lane_size.0 else {
        return;
    };
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }

    for (entity, parent) in existing_bodies.iter() {
        if parent.parent() == pitch_entity {
            commands.entity(entity).despawn();
        }
    }

    for segment in &scene.0.pitch_segments {
        for pair in segment.points.windows(2) {
            let Some(geom) = trace_segment_geom(pair[0].point, pair[1].point, size) else {
                continue;
            };
            let color =
                trace_color(((pair[0].confidence + pair[1].confidence) * 0.5).clamp(0.0, 1.0));
            commands.spawn((
                TraceSegmentBody,
                ChildOf(pitch_entity),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(geom.center.x - geom.length * 0.5),
                    top: px(geom.center.y - TRACE_WIDTH * 0.5),
                    width: px(geom.length),
                    height: px(TRACE_WIDTH),
                    ..default()
                },
                UiTransform::from_rotation(Rot2::radians(geom.angle)),
                BackgroundColor(color),
            ));
        }
    }
}

pub fn capture_pitch_lane_size(
    pitch_lane: Query<&ComputedNode, With<TimeGraphPitchLane>>,
    mut lane_size: ResMut<TimeGraphPitchLaneSize>,
) {
    let Ok(node) = pitch_lane.single() else {
        return;
    };
    let size = node.size();
    if lane_size.0 != Some(size) {
        lane_size.0 = Some(size);
    }
}

pub fn project_scene(graph: &SemanticGraph) -> TimeGraphScene {
    let Some(time_window) = graph.time_window else {
        return TimeGraphScene::default();
    };

    let onset_ticks = graph
        .onset_ticks
        .iter()
        .filter_map(|tick| normalize_onset_tick(*tick, time_window))
        .collect();
    let breath_spans = graph
        .breath_spans
        .iter()
        .filter_map(|span| normalize_breath_span(*span, time_window))
        .collect();
    let grooves = graph
        .pitch_window
        .map(|pitch_window| {
            graph
                .grooves
                .iter()
                .filter_map(|groove| normalize_groove(*groove, pitch_window))
                .collect()
        })
        .unwrap_or_default();
    let pitch_segments = graph
        .pitch_window
        .map(|pitch_window| {
            graph
                .trace_segments
                .iter()
                .filter_map(|segment| normalize_trace_segment(segment, time_window, pitch_window))
                .collect()
        })
        .unwrap_or_default();

    TimeGraphScene {
        pitch_segments,
        grooves,
        onset_ticks,
        breath_spans,
    }
}

fn normalize_trace_segment(
    segment: &TraceSegment,
    time_window: TimeWindow,
    pitch_window: PitchWindow,
) -> Option<NormalizedTraceSegment> {
    let points = segment
        .points
        .iter()
        .filter_map(|point| {
            Some(NormalizedTracePoint {
                point: NormalizedPoint {
                    x: normalize_time(point.t_ms, time_window)?,
                    y: normalize_pitch(point.pitch, pitch_window)?,
                },
                confidence: point.confidence,
                vibrato_rate: point.vibrato_rate,
                vibrato_depth: point.vibrato_depth,
            })
        })
        .collect::<Vec<_>>();
    (!points.is_empty()).then_some(NormalizedTraceSegment { points })
}

fn normalize_groove(groove: GrooveLine, pitch_window: PitchWindow) -> Option<NormalizedGrooveLine> {
    Some(NormalizedGrooveLine {
        y: normalize_pitch(groove.pitch, pitch_window)?,
        slot: groove.slot,
        active: groove.active,
    })
}

fn normalize_onset_tick(tick: OnsetTick, time_window: TimeWindow) -> Option<NormalizedOnsetTick> {
    Some(NormalizedOnsetTick {
        x: normalize_time(tick.t_ms, time_window)?,
        strength: tick.strength,
    })
}

fn normalize_breath_span(
    span: BreathSpan,
    time_window: TimeWindow,
) -> Option<NormalizedBreathSpan> {
    Some(NormalizedBreathSpan {
        x0: normalize_time(span.start_ms, time_window)?,
        x1: normalize_time(span.end_ms, time_window)?,
        peak: span.peak,
    })
}

fn normalize_time(t_ms: u64, window: TimeWindow) -> Option<f32> {
    let span = window.end_ms.saturating_sub(window.start_ms);
    if span == 0 {
        return None;
    }
    Some(((t_ms.saturating_sub(window.start_ms)) as f32 / span as f32).clamp(0.0, 1.0))
}

fn normalize_pitch(pitch: PitchLog2, window: PitchWindow) -> Option<f32> {
    let span = window.max.0 - window.min.0;
    if span <= 0.0 {
        return None;
    }
    Some(((pitch.0 - window.min.0) / span).clamp(0.0, 1.0))
}

fn normalized_to_lane(point: NormalizedPoint, size: Vec2) -> Vec2 {
    Vec2::new(size.x * point.x, size.y * (1.0 - point.y))
}

fn trace_segment_geom(
    start: NormalizedPoint,
    end: NormalizedPoint,
    lane_size: Vec2,
) -> Option<TraceSegmentGeom> {
    let start = normalized_to_lane(start, lane_size);
    let end = normalized_to_lane(end, lane_size);
    let delta = end - start;
    let length = delta.length();
    if length <= f32::EPSILON {
        return None;
    }
    Some(TraceSegmentGeom {
        center: (start + end) * 0.5,
        length,
        angle: delta.y.atan2(delta.x),
    })
}

fn trace_color(confidence: f32) -> Color {
    COLOR_TRACE.with_alpha(
        (TRACE_MIN_ALPHA + confidence * (TRACE_MAX_ALPHA - TRACE_MIN_ALPHA)).clamp(0.0, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_model::{
        BreathSpan, GrooveLine, OnsetTick, PitchWindow, SemanticGraph, TimeWindow, TracePoint,
        TraceSegment,
    };

    #[test]
    fn project_scene_normalizes_times_pitches_and_events() {
        let graph = SemanticGraph {
            time_window: Some(TimeWindow {
                start_ms: 10,
                end_ms: 110,
            }),
            pitch_window: Some(PitchWindow {
                min: PitchLog2(8.0),
                max: PitchLog2(10.0),
            }),
            trace_segments: vec![TraceSegment {
                points: vec![
                    TracePoint {
                        t_ms: 10,
                        pitch: PitchLog2(8.0),
                        confidence: 0.2,
                        vibrato_rate: 0.0,
                        vibrato_depth: 0.0,
                    },
                    TracePoint {
                        t_ms: 60,
                        pitch: PitchLog2(9.0),
                        confidence: 0.8,
                        vibrato_rate: 5.0,
                        vibrato_depth: 0.2,
                    },
                ],
            }],
            grooves: vec![GrooveLine {
                pitch: PitchLog2(9.0),
                slot: 3,
                active: true,
            }],
            onset_ticks: vec![OnsetTick {
                t_ms: 35,
                strength: 0.9,
            }],
            breath_spans: vec![BreathSpan {
                start_ms: 20,
                end_ms: 80,
                peak: 0.7,
            }],
        };

        let scene = project_scene(&graph);
        assert_eq!(scene.pitch_segments.len(), 1);
        assert_eq!(scene.grooves.len(), 1);
        assert_eq!(scene.onset_ticks.len(), 1);
        assert_eq!(scene.breath_spans.len(), 1);
        let point = scene.pitch_segments[0].points[1].point;
        assert!((point.x - 0.5).abs() < 1e-5);
        assert!((point.y - 0.5).abs() < 1e-5);
        assert!((scene.grooves[0].y - 0.5).abs() < 1e-5);
        assert!((scene.onset_ticks[0].x - 0.25).abs() < 1e-5);
        assert!((scene.breath_spans[0].x0 - 0.10).abs() < 1e-5);
        assert!((scene.breath_spans[0].x1 - 0.70).abs() < 1e-5);
    }

    #[test]
    fn trace_segment_geom_maps_lane_local_screen_space() {
        let Some(geom) = trace_segment_geom(
            NormalizedPoint { x: 0.25, y: 0.25 },
            NormalizedPoint { x: 0.75, y: 0.75 },
            Vec2::new(200.0, 100.0),
        ) else {
            panic!("expected segment geometry");
        };
        assert!((geom.center.x - 100.0).abs() < 1e-5);
        assert!((geom.center.y - 50.0).abs() < 1e-5);
        assert!(geom.length > 0.0);
        assert!(geom.angle < 0.0);
    }
}
