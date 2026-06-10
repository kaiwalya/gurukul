//! Time-graph systems: Bevy node spawning, markers, painting, layout
//! capture. Reads the [scene](super::scene), knows the engine, not the
//! domain.

use bevy::prelude::*;
use bevy::ui::ComputedNode;

use super::scene::{LogicalSize, NormalizedPoint, TimeGraphPitchLaneSize, TimeGraphSceneRes};

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

/// Spawn the time-graph root and its two lanes under `parent`, returning
/// the root entity — matching the widget-spawn template (the other slices'
/// `spawn` shape), so glue and isolated tests call it the same way.
pub fn spawn(commands: &mut Commands, parent: Entity) -> Entity {
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

    root
}

pub fn apply_scene(
    mut commands: Commands,
    scene: Res<TimeGraphSceneRes>,
    pitch_lane: Query<Entity, With<TimeGraphPitchLane>>,
    events_lane: Query<Entity, With<TimeGraphEventsLane>>,
    grooves: Query<Entity, With<GrooveLineMarker>>,
    onsets: Query<Entity, With<OnsetTickMarker>>,
    breaths: Query<Entity, With<BreathSpanMarker>>,
) {
    // Skip when nothing changed *and* this system's own markers are already
    // painted. The emptiness check is scoped to the markers `apply_scene`
    // owns — not the lane's `Children` — because the pitch lane is *shared*
    // with `apply_trace_scene`'s trace bodies; counting those as "already
    // painted" would wrongly suppress a first groove paint.
    let already_painted = !grooves.is_empty() && (!onsets.is_empty() || !breaths.is_empty());
    if !scene.is_changed() && already_painted {
        return;
    }

    let Ok(pitch_entity) = pitch_lane.single() else {
        return;
    };
    let Ok(events_entity) = events_lane.single() else {
        return;
    };

    // Despawn only what this system spawned. Clearing the pitch lane
    // wholesale (`despawn_related::<Children>()`) would also destroy the
    // trace bodies `apply_trace_scene` parents there — the shared-parent
    // despawn hazard in `ARCHITECTURE.md`. Marker-scoped despawn removes
    // the cross-system coupling that only `.chain()` ordering was hiding.
    for entity in grooves.iter().chain(onsets.iter()).chain(breaths.iter()) {
        commands.entity(entity).despawn();
    }

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
    let Some(size) = lane_size.0.map(LogicalSize::get) else {
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
    // Convert physical → logical here, once, behind the frame newtype, so
    // the paint system below speaks the same `px(...)` frame as `Node`.
    let size = LogicalSize::from_computed(node);
    if lane_size.0 != Some(size) {
        lane_size.0 = Some(size);
    }
}

/// Map a lane-local normalized point into logical lane pixels, insetting
/// the drawable area by the trace stroke's half-width on every side. The
/// trace is a stroke of width `TRACE_WIDTH` centered on its path, so a
/// centerline endpoint at a normalized 0 or 1 would draw half the stroke
/// past the lane edge. The lane's `Overflow::clip()` cannot rescue it: a
/// `UiTransform`-rotated body is distorted, not clipped, by Bevy's
/// axis-aligned clip (see `ARCHITECTURE.md` / the bug notes). So the
/// projection reserves the stroke margin itself — `[0, 1]` maps into
/// `[half, size - half]` — and the drawn extent stays inside by
/// construction, not by relying on a clip.
fn normalized_to_lane(point: NormalizedPoint, size: Vec2) -> Vec2 {
    let half = TRACE_WIDTH * 0.5;
    let drawable = (size - Vec2::splat(TRACE_WIDTH)).max(Vec2::ZERO);
    Vec2::new(
        half + drawable.x * point.x,
        half + drawable.y * (1.0 - point.y),
    )
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
