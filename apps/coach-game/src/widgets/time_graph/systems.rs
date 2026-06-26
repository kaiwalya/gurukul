//! Time-graph systems: Bevy node spawning, markers, painting, layout
//! capture. Reads the [scene](super::scene), knows the engine, not the
//! domain.

use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};

use super::scene::{
    LogicalSize, NormalizedPoint, TimeGraphGridSceneRes, TimeGraphLiveSceneRes,
    TimeGraphPitchLanePhysRect, TimeGraphPitchLaneScale, TimeGraphPitchLaneSize,
};

const LANE_GAP: f32 = 12.0;
const LANE_PADDING: f32 = 10.0;
const GROOVE_HEIGHT: f32 = 1.0;
const TICK_WIDTH: f32 = 2.0;
const TRACE_WIDTH: f32 = 3.0;

const COLOR_ROOT: Color = Color::srgba(0.08, 0.08, 0.11, 0.94);
const COLOR_PITCH_LANE: Color = Color::srgba(0.10, 0.10, 0.13, 0.96);
const COLOR_EVENT_LANE: Color = Color::srgba(0.11, 0.11, 0.14, 0.94);
const COLOR_GROOVE_ACTIVE: Color = Color::srgba(0.45, 0.70, 0.95, 0.08);
const COLOR_GROOVE_INACTIVE: Color = Color::srgba(0.56, 0.56, 0.63, 0.03);
const COLOR_ONSET: Color = Color::srgb(1.0, 0.86, 0.44);
const COLOR_BREATH: Color = Color::srgba(0.88, 0.64, 0.95, 0.34);
const COLOR_TRACE: Color = Color::srgb(0.94, 0.94, 0.98);
/// Coral/salmon tint shown where stable vibrato is detected. Intentionally
/// warmer and more saturated than the amber `COLOR_ONSET` so the two remain
/// visually separable at a glance.
const COLOR_VIBRATO: Color = Color::srgb(1.0, 0.45, 0.42);
const TRACE_MAX_ALPHA: f32 = 0.95;

#[derive(Component)]
pub struct TimeGraphRoot;

#[derive(Component)]
pub struct TimeGraphPitchLane;

#[derive(Component)]
pub struct TimeGraphEventsLane;

/// Back layer of the pitch lane: holds the tonal gridlines. A full-size
/// child of the lane, kept separate from [`TraceLayer`] so the two never
/// share a parent — the structural fix for the despawn-fight hazard (see
/// `ARCHITECTURE.md`). Each layer's painter clears only its own layer.
#[derive(Component)]
pub struct GridlineLayer;

/// Front layer of the pitch lane: holds the pitch trace bodies.
#[derive(Component)]
pub struct TraceLayer;

#[derive(Component)]
pub struct GrooveLineMarker;

#[derive(Component)]
pub struct OnsetTickMarker;

#[derive(Component)]
pub struct BreathSpanMarker;

#[derive(Component)]
pub struct TraceSegmentBody;

/// One segment's polyline data for the `poly` trace channel.
pub struct TraceSegmentSnapshot {
    /// Lane-local logical px points (one per segment point, not per pair).
    pub lane_logical: Vec<[f32; 2]>,
    /// Physical px AABB `[min_x, min_y, max_x, max_y]` of this segment's
    /// trace centerlines.
    pub aabb_px: [f32; 4],
    /// Post-clip AABB (intersection with lane physical rect).
    pub clipped_aabb_px: [f32; 4],
    /// Scale factor used for the conversion.
    pub scale_factor: f32,
}

/// Populated by `apply_trace` each frame with the lane-local polyline and
/// physical-px bounds for each trace segment. Read by `record_poly` in the
/// trace plugin. The resource is always initialized (even empty), so
/// `record_poly` can read it unconditionally.
#[derive(Resource, Default)]
pub struct LastTraceGeom(pub Vec<TraceSegmentSnapshot>);

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
            // `Name`s give the trace recorder a stable, run-to-run widget path
            // (`time_graph/pitch_lane/trace_layer/…`) instead of a volatile
            // `Entity` id. They also help any future inspector tooling.
            Name::new("time_graph"),
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                row_gap: px(LANE_GAP),
                padding: UiRect::all(px(LANE_PADDING)),
                ..default()
            },
            BackgroundColor(COLOR_ROOT),
        ))
        .id();

    commands.entity(root).with_children(|parent| {
        // The pitch lane is the measured frame owner (capture reads its
        // ComputedNode) and the clip boundary. It holds two full-size,
        // absolutely-positioned layer children — gridlines behind, trace in
        // front — so the two painters never share a parent.
        parent
            .spawn((
                TimeGraphPitchLane,
                Name::new("pitch_lane"),
                Node {
                    position_type: PositionType::Relative,
                    flex_grow: 4.0,
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(COLOR_PITCH_LANE),
            ))
            .with_children(|lane| {
                lane.spawn((
                    GridlineLayer,
                    Name::new("gridline_layer"),
                    layer_node(),
                    ZIndex(0),
                ));
                lane.spawn((
                    TraceLayer,
                    Name::new("trace_layer"),
                    layer_node(),
                    ZIndex(1),
                ));
            });
        parent.spawn((
            TimeGraphEventsLane,
            Name::new("events_lane"),
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

/// A full-size, absolutely-positioned, transparent layer that fills its
/// parent lane. Both pitch-lane layers use it; back/front is decided by the
/// `ZIndex` each is spawned with.
fn layer_node() -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(0),
        top: px(0),
        right: px(0),
        bottom: px(0),
        ..default()
    }
}

/// Repaint the tonal gridlines into the gridline layer. Gated on the
/// slow-cadence grid scene, so it only runs when the grooves actually
/// change (the glue value-gates the write). The gridline layer is *not*
/// shared with the trace, so this can clear its own children wholesale.
pub fn apply_gridlines(
    mut commands: Commands,
    grid: Res<TimeGraphGridSceneRes>,
    layer: Query<Entity, With<GridlineLayer>>,
    grooves: Query<Entity, With<GrooveLineMarker>>,
) {
    let already_painted = !grooves.is_empty();
    if !grid.is_changed() && already_painted {
        return;
    }
    let Ok(layer_entity) = layer.single() else {
        return;
    };
    for entity in grooves.iter() {
        commands.entity(entity).despawn();
    }

    for groove in &grid.grooves {
        commands.entity(layer_entity).with_child((
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
}

/// Repaint the time-anchored event markers (onset ticks, breath spans) into
/// the events lane. Gated on the fast-cadence live scene — these scroll with
/// the rolling time window. The events lane has no shared-parent problem, so
/// no layer split is needed there.
pub fn apply_events(
    mut commands: Commands,
    live: Res<TimeGraphLiveSceneRes>,
    events_lane: Query<Entity, With<TimeGraphEventsLane>>,
    onsets: Query<Entity, With<OnsetTickMarker>>,
    breaths: Query<Entity, With<BreathSpanMarker>>,
) {
    let already_painted = !onsets.is_empty() || !breaths.is_empty();
    if !live.is_changed() && already_painted {
        return;
    }
    let Ok(events_entity) = events_lane.single() else {
        return;
    };
    for entity in onsets.iter().chain(breaths.iter()) {
        commands.entity(entity).despawn();
    }

    for onset in &live.onset_ticks {
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

    for span in &live.breath_spans {
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

/// Repaint the pitch trace into the trace layer. Gated on the fast-cadence
/// live scene plus the measured lane size (the trace geometry is in logical
/// pixels). The trace layer is its own entity, so the despawn is a simple
/// wholesale clear — no parent-filtering, no coupling to the gridlines.
#[allow(clippy::too_many_arguments)]
pub fn apply_trace(
    mut commands: Commands,
    live: Res<TimeGraphLiveSceneRes>,
    layer: Query<Entity, With<TraceLayer>>,
    lane_size: Res<TimeGraphPitchLaneSize>,
    existing_bodies: Query<Entity, With<TraceSegmentBody>>,
    mut last_geom: ResMut<LastTraceGeom>,
    lane_phys_rect: Res<TimeGraphPitchLanePhysRect>,
    scale_res: Res<TimeGraphPitchLaneScale>,
) {
    let has_existing = !existing_bodies.is_empty();
    if !live.is_changed() && has_existing {
        return;
    }
    let Ok(layer_entity) = layer.single() else {
        return;
    };
    let Some(size) = lane_size.0.map(LogicalSize::get) else {
        return;
    };
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }

    // Clear last frame's poly data now that we know we are repainting.
    last_geom.0.clear();

    for entity in existing_bodies.iter() {
        commands.entity(entity).despawn();
    }

    for segment in &live.pitch_segments {
        // Poly channel: collect lane-local logical px for all points (not just pairs).
        let lane_logical: Vec<[f32; 2]> = segment
            .points
            .iter()
            .map(|tp| {
                let p = normalized_to_lane(tp.point, size);
                [p.x, p.y]
            })
            .collect();

        if !lane_logical.is_empty() {
            let scale_factor = scale_res.0;
            let aabb_px = if let Some(rect) = lane_phys_rect.0 {
                let origin = [rect[0], rect[1]];
                segment_phys_aabb(&lane_logical, scale_factor, origin)
            } else {
                [0.0, 0.0, 0.0, 0.0]
            };
            let clipped_aabb_px = if let Some(rect) = lane_phys_rect.0 {
                intersect_aabb(aabb_px, rect)
            } else {
                aabb_px
            };
            last_geom.0.push(TraceSegmentSnapshot {
                lane_logical,
                aabb_px,
                clipped_aabb_px,
                scale_factor,
            });
        }

        for pair in segment.points.windows(2) {
            let Some(geom) = trace_segment_geom(pair[0].point, pair[1].point, size) else {
                continue;
            };
            let avg_confidence = ((pair[0].confidence + pair[1].confidence) * 0.5).clamp(0.0, 1.0);
            let avg_vibrato =
                ((pair[0].vibrato_strength + pair[1].vibrato_strength) * 0.5).clamp(0.0, 1.0);
            let color = trace_color(avg_confidence, avg_vibrato);
            commands.spawn((
                TraceSegmentBody,
                ChildOf(layer_entity),
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
    pitch_lane: Query<(&ComputedNode, &UiGlobalTransform), With<TimeGraphPitchLane>>,
    mut lane_size: ResMut<TimeGraphPitchLaneSize>,
    mut lane_phys_rect: ResMut<TimeGraphPitchLanePhysRect>,
    mut lane_scale: ResMut<TimeGraphPitchLaneScale>,
) {
    let Ok((node, xform)) = pitch_lane.single() else {
        return;
    };
    // Convert physical → logical here, once, behind the frame newtype, so
    // the paint system below speaks the same `px(...)` frame as `Node`.
    let size = LogicalSize::from_computed(node);
    if lane_size.0 != Some(size) {
        lane_size.0 = Some(size);
    }
    // Physical rect: center from global affine, corners from physical size.
    let phys_size = node.size(); // physical px
    let center = xform.affine().transform_point2(Vec2::ZERO);
    let half = phys_size * 0.5;
    let rect = [
        center.x - half.x,
        center.y - half.y,
        center.x + half.x,
        center.y + half.y,
    ];
    lane_phys_rect.0 = Some(rect);
    // Scale factor: 1 / inverse_scale_factor (guard against zero).
    let sf = if node.inverse_scale_factor() > 0.0 {
        1.0 / node.inverse_scale_factor()
    } else {
        1.0
    };
    lane_scale.0 = sf;
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

/// Compute the physical-px AABB of a set of lane-local logical-px polyline
/// points, expanded by the trace stroke's half-width.
pub fn segment_phys_aabb(
    lane_logical: &[[f32; 2]],
    scale_factor: f32,
    lane_origin: [f32; 2],
) -> [f32; 4] {
    let half_stroke = TRACE_WIDTH * scale_factor * 0.5;
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for p in lane_logical {
        let px = p[0] * scale_factor + lane_origin[0];
        let py = p[1] * scale_factor + lane_origin[1];
        if px < min_x {
            min_x = px;
        }
        if py < min_y {
            min_y = py;
        }
        if px > max_x {
            max_x = px;
        }
        if py > max_y {
            max_y = py;
        }
    }
    [
        min_x - half_stroke,
        min_y - half_stroke,
        max_x + half_stroke,
        max_y + half_stroke,
    ]
}

/// Intersect two AABBs `[min_x, min_y, max_x, max_y]`. Returns a degenerate
/// (min > max) rect if they do not overlap.
pub fn intersect_aabb(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

fn trace_color(confidence: f32, vibrato_strength: f32) -> Color {
    // Match the note-dial needle: confidence drives alpha through a 4th-power
    // curve (`note_dial::model::project_needle`), fading the trace all the way
    // to invisible at noise-floor confidence (no alpha floor) while confident
    // voice stays solid. Weak pitch leaves an invisible gap in the line, just
    // as the needle vanishes when unsure.
    let conf = confidence.clamp(0.0, 1.0).powi(4);
    let alpha = (conf * TRACE_MAX_ALPHA).clamp(0.0, 1.0);
    COLOR_TRACE
        .mix(&COLOR_VIBRATO, vibrato_strength)
        .with_alpha(alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_to_lane_corners() {
        let size = Vec2::new(100.0, 80.0);
        let half = TRACE_WIDTH * 0.5;
        let drawable = size - Vec2::splat(TRACE_WIDTH);
        // Top of the normalized range (y=1) maps to the top of the drawable area.
        let top_left = normalized_to_lane(NormalizedPoint { x: 0.0, y: 1.0 }, size);
        assert!((top_left.x - half).abs() < 1e-5, "x at normalized 0");
        assert!((top_left.y - half).abs() < 1e-5, "y at normalized top");
        // Bottom of normalized range (y=0) maps to the bottom.
        let bot_right = normalized_to_lane(NormalizedPoint { x: 1.0, y: 0.0 }, size);
        assert!(
            (bot_right.x - (half + drawable.x)).abs() < 1e-5,
            "x at normalized 1"
        );
        assert!(
            (bot_right.y - (half + drawable.y)).abs() < 1e-5,
            "y at normalized bottom"
        );
    }

    #[test]
    fn segment_phys_aabb_basic() {
        // Single point at lane-local logical [1.5, 1.5], scale 2.0, lane origin [10.0, 20.0]:
        // phys pt = [1.5*2 + 10, 1.5*2 + 20] = [13.0, 23.0]
        // half_stroke = TRACE_WIDTH * 2.0 * 0.5 = TRACE_WIDTH = 3.0
        let pts = vec![[1.5f32, 1.5f32]];
        let aabb = segment_phys_aabb(&pts, 2.0, [10.0, 20.0]);
        let half_stroke = TRACE_WIDTH * 2.0 * 0.5;
        assert!((aabb[0] - (13.0 - half_stroke)).abs() < 1e-4, "min_x");
        assert!((aabb[1] - (23.0 - half_stroke)).abs() < 1e-4, "min_y");
        assert!((aabb[2] - (13.0 + half_stroke)).abs() < 1e-4, "max_x");
        assert!((aabb[3] - (23.0 + half_stroke)).abs() < 1e-4, "max_y");
    }

    #[test]
    fn intersect_aabb_clipping() {
        let aabb = [5.0f32, 5.0, 15.0, 15.0];
        let lane = [0.0f32, 0.0, 10.0, 10.0];
        let clipped = intersect_aabb(aabb, lane);
        assert_eq!(clipped, [5.0, 5.0, 10.0, 10.0]);
    }

    #[test]
    fn intersect_aabb_fully_inside() {
        let aabb = [2.0f32, 2.0, 8.0, 8.0];
        let lane = [0.0f32, 0.0, 10.0, 10.0];
        let clipped = intersect_aabb(aabb, lane);
        assert_eq!(clipped, aabb);
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
