//! Time-graph systems: Bevy node spawning, markers, painting, layout
//! capture. Reads the [scene](super::scene), knows the engine, not the
//! domain.

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, MeshMaterial2d};
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
const TRACE_MAX_ALPHA: f32 = 0.95;
/// Desaturated blue-grey band shown behind the trace during vibrato. Neutral
/// enough to not compete with the trace or groove lines while still being
/// legible against the dark lane background.
const COLOR_BAND: Color = Color::srgba(0.55, 0.65, 0.82, 1.0);
/// Maximum alpha for the vibrato band at full vibrato strength.
const BAND_MAX_ALPHA: f32 = 0.22;

#[derive(Component)]
pub struct TimeGraphRoot;

#[derive(Component)]
pub struct TimeGraphPitchLane;

#[derive(Component)]
pub struct TimeGraphEventsLane;

/// Back layer of the pitch lane: holds the tonal gridlines.
#[derive(Component)]
pub struct GridlineLayer;

#[derive(Component)]
pub struct GrooveLineMarker;

#[derive(Component)]
pub struct OnsetTickMarker;

#[derive(Component)]
pub struct BreathSpanMarker;

/// Marker component for the overlay camera used by the mesh-trace path.
#[derive(Component)]
pub struct TraceMeshCamera;

/// Marker component for mesh-trace triangle-strip entities.
#[derive(Component)]
pub struct TraceMeshEntity;

/// Persistent handles for the single mesh-trace entity. The trace is one
/// entity whose `Mesh` asset is mutated in place every frame — never
/// despawned/respawned — so the trace never blinks out for a frame between a
/// despawn and the next spawn (the old per-frame churn flickered badly).
#[derive(Resource)]
pub struct TraceMeshHandles {
    pub entity: Entity,
    pub mesh: Handle<Mesh>,
    pub material: Handle<TraceMaterial>,
}

/// Marker component for the vibrato-band filled-ribbon mesh entity.
#[derive(Component)]
pub struct BandMeshEntity;

/// Persistent handles for the single vibrato-band mesh entity. Mirrors the
/// `TraceMeshHandles` pattern: one entity mutated in place, never respawned.
#[derive(Resource)]
pub struct BandMeshHandles {
    pub entity: Entity,
    pub mesh: Handle<Mesh>,
    pub material: Handle<TraceMaterial>,
}

/// Marker component for the 2d background-fill quad that replaces the opaque
/// UI lane background in the mesh-trace render path.
#[derive(Component)]
pub struct LaneBgMeshEntity;

/// Marker component for 2d horizontal gridline quads spawned by
/// [`apply_mesh_gridlines`].
#[derive(Component)]
pub struct GridlineMeshEntity;

/// GPU material for the pitch-lane interior meshes (trace, gridlines, and the
/// background fill). Clips to the pitch-lane rect in world space and applies
/// per-vertex colour. `params.x` toggles the right-edge fade: `1.0` for the
/// trace (its leading head dissolves), `0.0` for the opaque background fill and
/// the gridlines (full coverage, no see-through edge against the app
/// background).
#[derive(AsBindGroup, Asset, TypePath, Debug, Clone)]
pub struct TraceMaterial {
    /// `[min_x, min_y, max_x, max_y]` in world (y-up) coordinates.
    #[uniform(0)]
    pub clip_rect: Vec4,
    /// `x` = right-edge fade flag (1 = fade the leading edge, 0 = no fade).
    /// `y` = opaque flag (1 = opaque alpha-mode, 0 = blended). `zw` padding for
    /// 16-byte uniform alignment.
    #[uniform(1)]
    pub params: Vec4,
}

impl TraceMaterial {
    /// The trace material: right-edge fade on, alpha-blended (confidence alpha).
    fn trace(clip_rect: Vec4) -> Self {
        Self {
            clip_rect,
            params: Vec4::new(1.0, 0.0, 0.0, 0.0),
        }
    }

    /// The opaque background fill: no fade, opaque alpha-mode, so its edges
    /// fully cover and never let the moving app background flicker through.
    fn fill(clip_rect: Vec4) -> Self {
        Self {
            clip_rect,
            params: Vec4::new(0.0, 1.0, 0.0, 0.0),
        }
    }

    /// The gridline material: no fade, blended (gridlines are intentionally
    /// faint, so they keep their low alpha).
    fn gridline(clip_rect: Vec4) -> Self {
        Self {
            clip_rect,
            params: Vec4::ZERO,
        }
    }

    fn opaque(&self) -> bool {
        self.params.y > 0.5
    }
}

impl Material2d for TraceMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://coach_game/widgets/time_graph/trace.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        // The fill (background + gridlines) is opaque so its edges fully cover
        // the app background — a blended fill shimmers against the moving
        // backdrop behind the overlay camera. Only the trace blends (for its
        // confidence-driven alpha and head fade).
        if self.opaque() {
            AlphaMode2d::Opaque
        } else {
            AlphaMode2d::Blend
        }
    }
}

/// Pitch-lane bounds in Bevy world space (y-up, origin at window centre).
#[derive(Debug, Clone, Copy)]
pub struct LaneWorldRect {
    /// `[min_x, min_y, max_x, max_y]` in world space.
    pub world: [f32; 4],
}

impl LaneWorldRect {
    /// Convert a physical-px lane rect (from [`TimeGraphPitchLanePhysRect`]) to
    /// world space.  `window_logical` is the window size in logical pixels.
    pub fn from_phys(phys_rect: [f32; 4], scale_factor: f32, window_logical: Vec2) -> Self {
        let sf = scale_factor.max(f32::EPSILON);
        let lx0 = phys_rect[0] / sf;
        let ly0 = phys_rect[1] / sf;
        let lx1 = phys_rect[2] / sf;
        let ly1 = phys_rect[3] / sf;
        let hw = window_logical.x * 0.5;
        let hh = window_logical.y * 0.5;
        // UI y is top-down; world y is up.
        let wx0 = lx0 - hw;
        let wy0 = hh - ly1;
        let wx1 = lx1 - hw;
        let wy1 = hh - ly0;
        Self {
            world: [wx0, wy0, wx1, wy1],
        }
    }

    /// Convert a lane-local logical-px point (origin top-left, y-down) to world
    /// space.
    pub fn lane_local_to_world(&self, local: Vec2) -> Vec2 {
        Vec2::new(self.world[0] + local.x, self.world[3] - local.y)
    }

    /// Return the clip rect as a `Vec4` suitable for a uniform binding.
    pub fn clip_rect_uniform(&self) -> Vec4 {
        Vec4::new(self.world[0], self.world[1], self.world[2], self.world[3])
    }
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

const HALF_WIDTH: f32 = 1.5;
const MITER_LIMIT_COS: f32 = 0.342; // cos(70°)

/// Append one polyline segment's triangles into shared mesh buffers. Each
/// point-pair becomes a quad expanded by ±[`HALF_WIDTH`]; sharp joints get a
/// bevel triangle. A no-op when fewer than two points are supplied. Indices are
/// emitted relative to the current `positions` length, so several segments can
/// share one mesh (each its own disjoint strip — no spurious bridge between the
/// gaps where pitch was unvoiced).
fn append_trace_segment(
    world_points: &[Vec2],
    colors: &[[f32; 4]],
    positions: &mut Vec<[f32; 3]>,
    vertex_colors: &mut Vec<[f32; 4]>,
    indices: &mut Vec<u32>,
) {
    if world_points.len() < 2 {
        return;
    }
    let n = world_points.len();

    fn perp(d: Vec2) -> Vec2 {
        Vec2::new(-d.y, d.x)
    }

    let dirs: Vec<Vec2> = (0..n - 1)
        .map(|i| {
            let d = world_points[i + 1] - world_points[i];
            if d.length_squared() < f32::EPSILON {
                Vec2::X
            } else {
                d.normalize()
            }
        })
        .collect();

    let mut base: u32 = positions.len() as u32;
    for seg in 0..n - 1 {
        let p0 = world_points[seg];
        let p1 = world_points[seg + 1];
        let dir = dirs[seg];
        let norm = perp(dir);

        let start_offset = if seg == 0 {
            norm * HALF_WIDTH
        } else {
            let prev_norm = perp(dirs[seg - 1]);
            let miter = (norm + prev_norm).normalize_or(norm);
            let dot = norm.dot(prev_norm);
            if dot < MITER_LIMIT_COS {
                norm * HALF_WIDTH
            } else {
                let miter_len = HALF_WIDTH / miter.dot(norm).max(0.1);
                miter * miter_len
            }
        };

        let end_offset = if seg == n - 2 {
            norm * HALF_WIDTH
        } else {
            let next_norm = perp(dirs[seg + 1]);
            let miter = (norm + next_norm).normalize_or(norm);
            let dot = norm.dot(next_norm);
            if dot < MITER_LIMIT_COS {
                norm * HALF_WIDTH
            } else {
                let miter_len = HALF_WIDTH / miter.dot(norm).max(0.1);
                miter * miter_len
            }
        };

        let c0 = colors[seg];
        let c1 = colors[seg + 1];
        let v0 = p0 + start_offset;
        let v1 = p0 - start_offset;
        let v2 = p1 + end_offset;
        let v3 = p1 - end_offset;

        positions.extend([
            [v0.x, v0.y, 0.2],
            [v1.x, v1.y, 0.2],
            [v2.x, v2.y, 0.2],
            [v3.x, v3.y, 0.2],
        ]);
        vertex_colors.extend([c0, c0, c1, c1]);
        indices.extend([base, base + 1, base + 2, base + 1, base + 3, base + 2]);
        base += 4;

        // Bevel fill at sharp joints.
        if seg < n - 2 {
            let next_norm = perp(dirs[seg + 1]);
            let dot = norm.dot(next_norm);
            if dot < MITER_LIMIT_COS {
                let cross = dir.perp_dot(dirs[seg + 1]);
                let p_center = world_points[seg + 1];
                let bc = colors[seg + 1];
                if cross > 0.0 {
                    let next_offset = next_norm * HALF_WIDTH;
                    let ba = p_center + end_offset;
                    let bb = p_center + next_offset;
                    let bc_pt = p_center;
                    positions.extend([
                        [ba.x, ba.y, 0.2],
                        [bb.x, bb.y, 0.2],
                        [bc_pt.x, bc_pt.y, 0.2],
                    ]);
                    vertex_colors.extend([bc, bc, bc]);
                    indices.extend([base, base + 1, base + 2]);
                } else {
                    let next_offset = next_norm * HALF_WIDTH;
                    let ba = p_center - end_offset;
                    let bb = p_center - next_offset;
                    let bc_pt = p_center;
                    positions.extend([
                        [ba.x, ba.y, 0.2],
                        [bb.x, bb.y, 0.2],
                        [bc_pt.x, bc_pt.y, 0.2],
                    ]);
                    vertex_colors.extend([bc, bc, bc]);
                    indices.extend([base, base + 2, base + 1]);
                }
                base += 3;
            }
        }
    }
}

/// Build a single triangle-list [`Mesh`] for one polyline in world space.
/// Thin wrapper over [`append_trace_segment`] for isolated/test use.
/// Returns `None` if fewer than two points are supplied.
pub fn build_trace_mesh(world_points: &[Vec2], colors: &[[f32; 4]]) -> Option<Mesh> {
    if world_points.len() < 2 {
        return None;
    }
    let n = world_points.len();
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(n * 4);
    let mut vertex_colors: Vec<[f32; 4]> = Vec::with_capacity(n * 4);
    let mut indices: Vec<u32> = Vec::with_capacity((n - 1) * 6);
    append_trace_segment(
        world_points,
        colors,
        &mut positions,
        &mut vertex_colors,
        &mut indices,
    );
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vertex_colors);
    mesh.insert_indices(Indices::U32(indices));
    Some(mesh)
}

/// Drop the persistent trace-mesh handles on leaving InGame. The entity itself
/// is removed by its `DespawnOnExit`, so the stored `Entity` would dangle; clear
/// the resource so the next InGame session recreates the entity fresh.
pub fn clear_trace_mesh_handles(mut commands: Commands) {
    commands.remove_resource::<TraceMeshHandles>();
}

/// Build a filled ribbon [`Mesh`] representing the vibrato band. For each
/// adjacent pair of points, two triangles connect (top_i, bottom_i,
/// top_{i+1}, bottom_{i+1}). Colors encode vibrato strength via alpha.
/// Returns `None` if fewer than two points are supplied.
pub fn build_band_mesh(
    world_centers: &[Vec2],
    half_heights_world: &[f32],
    colors: &[[f32; 4]],
) -> Option<Mesh> {
    if world_centers.len() < 2 {
        return None;
    }
    let n = world_centers.len();
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(n * 2);
    let mut vertex_colors: Vec<[f32; 4]> = Vec::with_capacity(n * 2);
    let mut indices: Vec<u32> = Vec::with_capacity((n - 1) * 6);

    for i in 0..n {
        let cx = world_centers[i].x;
        let cy = world_centers[i].y;
        let hh = half_heights_world[i];
        positions.push([cx, cy + hh, 0.15]);
        positions.push([cx, cy - hh, 0.15]);
        vertex_colors.push(colors[i]);
        vertex_colors.push(colors[i]);
    }

    for i in 0..n - 1 {
        let base = (i * 2) as u32;
        // top_i, bottom_i, top_{i+1}
        indices.extend([base, base + 1, base + 2]);
        // bottom_i, bottom_{i+1}, top_{i+1}
        indices.extend([base + 1, base + 3, base + 2]);
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vertex_colors);
    mesh.insert_indices(Indices::U32(indices));
    Some(mesh)
}

/// Drop the persistent band-mesh handles on leaving InGame.
pub fn clear_band_mesh_handles(mut commands: Commands) {
    commands.remove_resource::<BandMeshHandles>();
}

/// Spawn the overlay [`Camera2d`] that renders mesh-trace entities on
/// [`RenderLayers::layer(1)`].
pub fn spawn_trace_overlay_camera(mut commands: Commands) {
    commands.spawn((
        TraceMeshCamera,
        Camera2d,
        Camera {
            order: 1,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::layer(1),
        DespawnOnExit(crate::state::AppState::InGame),
    ));
}

/// Rebuild the mesh-trace polyline whenever the live scene changes. All
/// [`NormalizedTraceSegment`]s are flattened into ONE mesh (each its own
/// disjoint strip) carried by a single persistent [`TraceMeshEntity`]. The
/// mesh asset is mutated in place — the entity is created once and never
/// despawned — so the trace can't blink out for a frame the way the old
/// despawn/respawn-per-frame churn did.
#[allow(clippy::too_many_arguments)]
pub fn apply_mesh_trace(
    mut commands: Commands,
    live: Res<TimeGraphLiveSceneRes>,
    lane_size: Res<TimeGraphPitchLaneSize>,
    lane_phys_rect: Res<TimeGraphPitchLanePhysRect>,
    scale_res: Res<TimeGraphPitchLaneScale>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    handles: Option<Res<TraceMeshHandles>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TraceMaterial>>,
) {
    // Only rebuild when the scene actually changed — once the entity exists.
    if !live.is_changed() && handles.is_some() {
        return;
    }

    let Some(size) = lane_size.0.map(LogicalSize::get) else {
        return;
    };
    let Some(phys_rect) = lane_phys_rect.0 else {
        return;
    };
    let scale_factor = scale_res.0;
    let Ok(window) = windows.single() else {
        return;
    };
    let window_logical = Vec2::new(window.width(), window.height());

    let lane_world = LaneWorldRect::from_phys(phys_rect, scale_factor, window_logical);
    let clip_rect = lane_world.clip_rect_uniform();

    // Flatten every segment into one set of buffers.
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut vertex_colors: Vec<[f32; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for segment in &live.pitch_segments {
        if segment.points.len() < 2 {
            continue;
        }
        let world_points: Vec<Vec2> = segment
            .points
            .iter()
            .map(|tp| {
                let local = normalized_to_lane(tp.point, size);
                lane_world.lane_local_to_world(local)
            })
            .collect();
        let colors: Vec<[f32; 4]> = segment
            .points
            .iter()
            .map(|tp| {
                let conf = tp.confidence.clamp(0.0, 1.0).powi(4);
                let alpha = (conf * TRACE_MAX_ALPHA).clamp(0.0, 1.0);
                let c = COLOR_TRACE.with_alpha(alpha);
                // ATTRIBUTE_COLOR is linear-rgb in the mesh2d pipeline; feed
                // linear so the trace colour matches the UI rectangle path.
                let lin = c.to_linear();
                [lin.red, lin.green, lin.blue, lin.alpha]
            })
            .collect();
        append_trace_segment(
            &world_points,
            &colors,
            &mut positions,
            &mut vertex_colors,
            &mut indices,
        );
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vertex_colors);
    mesh.insert_indices(Indices::U32(indices));

    match handles.as_deref() {
        // Entity exists: overwrite the mesh in place and refresh the clip rect.
        Some(h) => {
            // We hold a strong handle, so insert cannot fail; ignore the Result.
            let _ = meshes.insert(&h.mesh, mesh);
            if let Some(mat) = materials.get_mut(&h.material) {
                mat.clip_rect = clip_rect;
            }
        }
        // First run: create the single persistent entity + handles.
        None => {
            let mesh_handle = meshes.add(mesh);
            let mat_handle = materials.add(TraceMaterial::trace(clip_rect));
            let entity = commands
                .spawn((
                    TraceMeshEntity,
                    Mesh2d(mesh_handle.clone()),
                    MeshMaterial2d(mat_handle.clone()),
                    // `Mesh2d` only requires `Transform`, not `Visibility`. The
                    // mesh2d render extraction skips any entity whose
                    // `ViewVisibility` is unset/false, so an explicit
                    // `Visibility::Visible` is needed or the trace is culled
                    // before it rasterises.
                    Visibility::Visible,
                    RenderLayers::layer(1),
                    DespawnOnExit(crate::state::AppState::InGame),
                ))
                .id();
            commands.insert_resource(TraceMeshHandles {
                entity,
                mesh: mesh_handle,
                material: mat_handle,
            });
        }
    }
}

/// Rebuild the vibrato-band filled ribbon whenever the live scene changes.
/// All segments are flattened into ONE mesh carried by a single persistent
/// [`BandMeshEntity`], mutated in place — same pattern as `apply_mesh_trace`.
/// Z = 0.15, between gridlines (0.1) and the trace (0.2).
#[allow(clippy::too_many_arguments)]
pub fn apply_mesh_band(
    mut commands: Commands,
    live: Res<TimeGraphLiveSceneRes>,
    lane_size: Res<TimeGraphPitchLaneSize>,
    lane_phys_rect: Res<TimeGraphPitchLanePhysRect>,
    scale_res: Res<TimeGraphPitchLaneScale>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    handles: Option<Res<BandMeshHandles>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TraceMaterial>>,
) {
    if !live.is_changed() && handles.is_some() {
        return;
    }

    let Some(size) = lane_size.0.map(LogicalSize::get) else {
        return;
    };
    let Some(phys_rect) = lane_phys_rect.0 else {
        return;
    };
    let scale_factor = scale_res.0;
    let Ok(window) = windows.single() else {
        return;
    };
    let window_logical = Vec2::new(window.width(), window.height());
    let lane_world = LaneWorldRect::from_phys(phys_rect, scale_factor, window_logical);
    let clip_rect = lane_world.clip_rect_uniform();
    let lane_height = lane_world.world[3] - lane_world.world[1];

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut vertex_colors: Vec<[f32; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    let lin_band = COLOR_BAND.to_linear();

    for segment in &live.pitch_segments {
        if segment.points.len() < 2 {
            continue;
        }
        let n = segment.points.len();
        let seg_base = (positions.len() / 2) as u32;
        for tp in segment.points.iter() {
            let local = normalized_to_lane(tp.point, size);
            let world_center = lane_world.lane_local_to_world(local);
            let hh_world = tp.band_half_height * lane_height;
            let alpha = (tp.vibrato_strength * BAND_MAX_ALPHA).clamp(0.0, 1.0);
            let c = [lin_band.red, lin_band.green, lin_band.blue, alpha];

            positions.push([world_center.x, world_center.y + hh_world, 0.15]);
            positions.push([world_center.x, world_center.y - hh_world, 0.15]);
            vertex_colors.push(c);
            vertex_colors.push(c);
        }
        for i in 0..n - 1 {
            let b = seg_base + (i as u32) * 2;
            indices.extend([b, b + 1, b + 2, b + 1, b + 3, b + 2]);
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vertex_colors);
    mesh.insert_indices(Indices::U32(indices));

    match handles.as_deref() {
        Some(h) => {
            let _ = meshes.insert(&h.mesh, mesh);
            if let Some(mat) = materials.get_mut(&h.material) {
                mat.clip_rect = clip_rect;
            }
        }
        None => {
            let mesh_handle = meshes.add(mesh);
            let mat_handle = materials.add(TraceMaterial::trace(clip_rect));
            let entity = commands
                .spawn((
                    BandMeshEntity,
                    Mesh2d(mesh_handle.clone()),
                    MeshMaterial2d(mat_handle.clone()),
                    Visibility::Visible,
                    RenderLayers::layer(1),
                    DespawnOnExit(crate::state::AppState::InGame),
                ))
                .id();
            commands.insert_resource(BandMeshHandles {
                entity,
                mesh: mesh_handle,
                material: mat_handle,
            });
        }
    }
}

/// Make the pitch-lane UI background transparent so the 2d mesh layer shows
/// through. Runs every frame (cheap, idempotent). Also clears the root
/// background so the lane rect is fully visible.
pub fn clear_pitch_lane_bg_for_mesh(
    mut pitch_lanes: Query<&mut BackgroundColor, With<TimeGraphPitchLane>>,
    mut roots: Query<&mut BackgroundColor, (With<TimeGraphRoot>, Without<TimeGraphPitchLane>)>,
) {
    for mut bg in pitch_lanes.iter_mut() {
        bg.0 = Color::NONE;
    }
    for mut bg in roots.iter_mut() {
        bg.0 = Color::NONE;
    }
}

/// Spawn a 2d background-fill quad covering the full lane rect at Z=0.0, using
/// `TraceMaterial` so it shares the same clip/layer as the trace mesh.
/// Despawns and respawns whenever the lane rect changes.
pub fn apply_mesh_lane_bg(
    mut commands: Commands,
    lane_phys_rect: Res<TimeGraphPitchLanePhysRect>,
    scale_res: Res<TimeGraphPitchLaneScale>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    existing: Query<Entity, With<LaneBgMeshEntity>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TraceMaterial>>,
) {
    // Only rebuild when the lane rect has changed.
    if !lane_phys_rect.is_changed() && !existing.is_empty() {
        return;
    }

    for entity in existing.iter() {
        commands.entity(entity).despawn();
    }

    let Some(phys_rect) = lane_phys_rect.0 else {
        return;
    };
    let scale_factor = scale_res.0;
    let Ok(window) = windows.single() else {
        return;
    };
    let window_logical = Vec2::new(window.width(), window.height());
    let lane_world = LaneWorldRect::from_phys(phys_rect, scale_factor, window_logical);
    let clip_rect = lane_world.clip_rect_uniform();

    let [wx0, wy0, wx1, wy1] = lane_world.world;
    // ATTRIBUTE_COLOR is consumed as LINEAR rgb by the mesh2d pipeline, so feed
    // linear components (not sRGB) or the fill renders washed-out grey.
    let lin = COLOR_PITCH_LANE.to_linear();
    let c = [lin.red, lin.green, lin.blue, 1.0f32];

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_POSITION,
        vec![
            [wx0, wy0, 0.0f32],
            [wx1, wy0, 0.0f32],
            [wx1, wy1, 0.0f32],
            [wx0, wy1, 0.0f32],
        ],
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vec![c; 4]);
    mesh.insert_indices(Indices::U32(vec![0, 1, 2, 0, 2, 3]));

    commands.spawn((
        LaneBgMeshEntity,
        Mesh2d(meshes.add(mesh)),
        MeshMaterial2d(materials.add(TraceMaterial::fill(clip_rect))),
        Visibility::Visible,
        RenderLayers::layer(1),
        DespawnOnExit(crate::state::AppState::InGame),
    ));
}

/// Draw each groove line as a thin horizontal 2d quad at Z=0.1.
/// Despawns and respawns whenever [`TimeGraphGridSceneRes`] changes.
#[allow(clippy::too_many_arguments)]
pub fn apply_mesh_gridlines(
    mut commands: Commands,
    grid: Res<TimeGraphGridSceneRes>,
    lane_phys_rect: Res<TimeGraphPitchLanePhysRect>,
    scale_res: Res<TimeGraphPitchLaneScale>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    existing: Query<Entity, With<GridlineMeshEntity>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TraceMaterial>>,
) {
    let already_painted = !existing.is_empty();
    if !grid.is_changed() && already_painted {
        return;
    }

    for entity in existing.iter() {
        commands.entity(entity).despawn();
    }

    let Some(phys_rect) = lane_phys_rect.0 else {
        return;
    };
    let scale_factor = scale_res.0;
    let Ok(window) = windows.single() else {
        return;
    };
    let window_logical = Vec2::new(window.width(), window.height());
    let lane_world = LaneWorldRect::from_phys(phys_rect, scale_factor, window_logical);
    let clip_rect = lane_world.clip_rect_uniform();

    let [wx0, wy0, wx1, _wy1] = lane_world.world;
    let lane_height = lane_world.world[3] - lane_world.world[1];

    for groove in &grid.grooves {
        // groove.y is normalized [0,1] where 1 = top of lane (mirrors the UI
        // path's `top: percent((1.0 - groove.y)*100)`). World y is up: wy1 is
        // the top, wy0 the bottom, so groove.y=1 maps to wy1.
        let center_y = wy0 + groove.y.clamp(0.0, 1.0) * lane_height;
        let half_h = GROOVE_HEIGHT * 0.5;

        let color = if groove.active {
            COLOR_GROOVE_ACTIVE
        } else {
            COLOR_GROOVE_INACTIVE
        };
        // ATTRIBUTE_COLOR is linear-rgb in the mesh2d pipeline.
        let lin = color.to_linear();
        let c = [lin.red, lin.green, lin.blue, lin.alpha];

        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        );
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_POSITION,
            vec![
                [wx0, center_y - half_h, 0.1f32],
                [wx1, center_y - half_h, 0.1f32],
                [wx1, center_y + half_h, 0.1f32],
                [wx0, center_y + half_h, 0.1f32],
            ],
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vec![c; 4]);
        mesh.insert_indices(Indices::U32(vec![0, 1, 2, 0, 2, 3]));

        commands.spawn((
            GridlineMeshEntity,
            Mesh2d(meshes.add(mesh)),
            MeshMaterial2d(materials.add(TraceMaterial::gridline(clip_rect))),
            Visibility::Visible,
            RenderLayers::layer(1),
            DespawnOnExit(crate::state::AppState::InGame),
        ));
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

    // ── Mesh generator tests ───────────────────────────────────────────────

    /// A straight horizontal 3-point line should produce a pair of quads whose
    /// vertices are exactly ±HALF_WIDTH away from the centerline (no miter
    /// distortion on a straight run — the miter vector is identical to the
    /// normal, so no scaling occurs).
    #[test]
    fn mesh_straight_line_is_rectangle_equivalent() {
        use bevy::mesh::VertexAttributeValues;
        let pts = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(20.0, 0.0),
        ];
        let white = [1.0f32, 1.0, 1.0, 1.0];
        let colors = vec![white; 3];
        let mesh = build_trace_mesh(&pts, &colors).expect("expected mesh");

        // Extract y-coords of all vertex positions.
        let positions = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap();
        let VertexAttributeValues::Float32x3(pos) = positions else {
            panic!("wrong position format");
        };
        // Every y should be ±HALF_WIDTH (1.5) from the centerline (y=0).
        for p in pos {
            let y = p[1];
            assert!(
                (y - HALF_WIDTH).abs() < 1e-4 || (y + HALF_WIDTH).abs() < 1e-4,
                "unexpected y offset {y}; expected ±{HALF_WIDTH}"
            );
        }
    }

    /// A single-point input produces no mesh (fewer than 2 points).
    #[test]
    fn mesh_too_few_points_returns_none() {
        assert!(build_trace_mesh(&[Vec2::ZERO], &[[1.0; 4]]).is_none());
    }

    /// Per-vertex alpha equals conf⁴ × TRACE_MAX_ALPHA for known confidences.
    #[test]
    fn mesh_vertex_alpha_is_conf_fourth_times_max() {
        use bevy::mesh::VertexAttributeValues;
        for conf in [0.0f32, 0.5, 0.8, 1.0] {
            let expected_alpha = (conf.powi(4) * TRACE_MAX_ALPHA).clamp(0.0, 1.0);
            // Build a 2-point mesh where both points have the same confidence.
            // Color is assembled the same way apply_mesh_trace does it:
            // conf⁴ × TRACE_MAX_ALPHA → alpha channel.
            let pts = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)];
            let c_srgba = COLOR_TRACE.with_alpha(expected_alpha).to_srgba();
            let color = [c_srgba.red, c_srgba.green, c_srgba.blue, c_srgba.alpha];
            let colors = vec![color; 2];
            let mesh = build_trace_mesh(&pts, &colors).expect("expected mesh");
            let vc = mesh.attribute(Mesh::ATTRIBUTE_COLOR).unwrap();
            let VertexAttributeValues::Float32x4(cols) = vc else {
                panic!("wrong color format");
            };
            for c in cols {
                assert!(
                    (c[3] - expected_alpha).abs() < 1e-5,
                    "alpha mismatch: got {}, expected {expected_alpha}",
                    c[3]
                );
            }
        }
    }

    /// Centerline parity: the world-space mesh centerline must match the
    /// lane-local points produced by `normalized_to_lane`, converted through
    /// `LaneWorldRect::lane_local_to_world`.
    #[test]
    fn mesh_centerline_parity_matches_normalized_to_lane() {
        let size = Vec2::new(200.0, 100.0);
        // A 400×200 window at scale 2.0, lane starts at physical (20, 30).
        let window_logical = Vec2::new(200.0, 100.0);
        let phys_rect = [20.0f32, 30.0, 420.0, 230.0]; // 400×200 physical
        let scale_factor = 2.0f32;
        let lane_world = LaneWorldRect::from_phys(phys_rect, scale_factor, window_logical);

        let pts = [
            NormalizedPoint { x: 0.0, y: 1.0 },
            NormalizedPoint { x: 0.5, y: 0.5 },
            NormalizedPoint { x: 1.0, y: 0.0 },
        ];
        for p in &pts {
            let local = normalized_to_lane(*p, size);
            let world = lane_world.lane_local_to_world(local);
            // Reconstruct: local→world should round-trip cleanly.
            // world_x = lane_world.world[0] + local.x
            // world_y = lane_world.world[3] - local.y
            let expected_x = lane_world.world[0] + local.x;
            let expected_y = lane_world.world[3] - local.y;
            assert!(
                (world.x - expected_x).abs() < 1e-4,
                "world.x mismatch: {world:?} vs {expected_x}"
            );
            assert!(
                (world.y - expected_y).abs() < 1e-4,
                "world.y mismatch: {world:?} vs {expected_y}"
            );
        }
    }

    /// Clip AABB ⊆ lane: a polyline that overflows the right edge must still
    /// produce a clipped AABB fully inside the lane rect.
    #[test]
    fn clipped_aabb_is_subset_of_lane() {
        // Polyline that goes well past the right edge.
        let lane_logical: Vec<[f32; 2]> = vec![[5.0, 5.0], [200.0, 5.0]];
        let scale_factor = 2.0f32;
        let lane_origin = [0.0f32, 0.0];
        let lane_phys_rect = [0.0f32, 0.0, 100.0 * scale_factor, 50.0 * scale_factor];

        let aabb = segment_phys_aabb(&lane_logical, scale_factor, lane_origin);
        let clipped = intersect_aabb(aabb, lane_phys_rect);

        assert!(
            clipped[0] >= lane_phys_rect[0],
            "clipped min_x {0} < lane {1}",
            clipped[0],
            lane_phys_rect[0]
        );
        assert!(
            clipped[2] <= lane_phys_rect[2],
            "clipped max_x {0} > lane {1}",
            clipped[2],
            lane_phys_rect[2]
        );
        assert!(
            clipped[1] >= lane_phys_rect[1],
            "clipped min_y {0} < lane {1}",
            clipped[1],
            lane_phys_rect[1]
        );
        assert!(
            clipped[3] <= lane_phys_rect[3],
            "clipped max_y {0} > lane {1}",
            clipped[3],
            lane_phys_rect[3]
        );
    }

    /// `LaneWorldRect::from_phys` converts correctly: a known physical rect
    /// should produce known world bounds.
    ///
    /// Window: 400×200 logical, scale 2.0 → 800×400 physical.
    /// Lane physical rect: [0, 0, 800, 400] (fills the window).
    /// Expected world rect: [-200, -100, 200, 100] (centred at origin, y-up).
    #[test]
    fn lane_world_rect_conversion() {
        let phys = [0.0f32, 0.0, 800.0, 400.0];
        let rect = LaneWorldRect::from_phys(phys, 2.0, Vec2::new(400.0, 200.0));
        assert!((rect.world[0] - -200.0).abs() < 1e-4, "min_x");
        assert!((rect.world[1] - -100.0).abs() < 1e-4, "min_y");
        assert!((rect.world[2] - 200.0).abs() < 1e-4, "max_x");
        assert!((rect.world[3] - 100.0).abs() < 1e-4, "max_y");
    }

    /// A 2-point band segment with a known half-height produces top/bottom
    /// verts at center ± expected world offset. Center y = 0.0; half_height =
    /// 5.0 → top y = +5.0, bottom y = -5.0.
    #[test]
    fn build_band_mesh_top_bottom_offsets() {
        use bevy::mesh::VertexAttributeValues;
        let centers = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)];
        let half_heights = vec![5.0f32, 5.0f32];
        let white = [1.0f32, 1.0, 1.0, 1.0];
        let colors = vec![white; 2];
        let mesh = build_band_mesh(&centers, &half_heights, &colors).expect("expected mesh");

        let positions = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap();
        let VertexAttributeValues::Float32x3(pos) = positions else {
            panic!("wrong position format");
        };
        // 2 points × 2 verts = 4 vertices. Each pair should be ±5.0 in y.
        assert_eq!(pos.len(), 4);
        for p in pos {
            let y = p[1];
            assert!(
                (y - 5.0).abs() < 1e-4 || (y + 5.0).abs() < 1e-4,
                "unexpected y {y}; expected ±5.0"
            );
            // Z must be 0.15 (band layer, below trace at 0.2).
            assert!((p[2] - 0.15).abs() < 1e-4, "unexpected z {}", p[2]);
        }
    }

    /// Single-point input returns None (fewer than 2 points).
    #[test]
    fn build_band_mesh_too_few_points_returns_none() {
        assert!(build_band_mesh(&[Vec2::ZERO], &[1.0], &[[1.0; 4]]).is_none());
    }
}
