//! Bevy-side recording systems.
//!
//! Scheduled across the frame so each record lands in schedule order:
//! - `First`: the per-frame `frame` record (delta).
//! - `Update`: `input` (message readers), `mark` (F10), and the `coach`/`cmd`
//!   drain of the decorator buffer (ordered after `coach::drain_events`, the
//!   single reader of the handle), plus `state` transition records.
//! - `PostUpdate` after `UiSystems::PostLayout`: the `geom` on-change pass.
//! - `Last`: flush the writer.

use std::collections::HashMap;

use bevy::diagnostic::FrameCount;
use bevy::prelude::*;
use bevy::ui::{CalculatedClip, ComputedNode, UiGlobalTransform};
use bevy::window::WindowEvent;

use crate::state::AppState;

use super::record::{Body, CoachRead, GeomRecord, InputRecord, PolyRecord};
use super::recording_coach::TraceBufferHandle;
use super::writer::TraceWriter;

/// Running marker counter for F10 presses.
#[derive(Resource, Default)]
pub struct MarkerCounter(pub u32);

/// Previous frame's geometry hash per **widget path**, for on-change diffing and
/// vanish detection. Keyed by path, not `Entity`: the head respawns nodes (the
/// time-graph repaints its bodies every frame), and an entity key would read
/// each respawn as a vanish + reappear — churning ~half the trace with `gone`
/// records and a fresh full record for a node that didn't move. A path key
/// collapses respawn-in-place to nothing (same path, same hash → silent) while
/// still emitting `gone` when a path genuinely leaves the tree.
#[derive(Resource, Default)]
pub struct GeomMemory(HashMap<String, GeomSeen>);

struct GeomSeen {
    hash: u64,
    /// Raw entity id last seen at this path, supplementary only (the `entity`
    /// field of the record); never used for matching.
    entity: u64,
}

/// `First`: one `frame` record carrying the wall-time delta replay must
/// reproduce.
pub fn record_frame(mut writer: ResMut<TraceWriter>, frame: Res<FrameCount>, time: Res<Time>) {
    writer.write(
        frame.0,
        Body::Frame {
            delta_s: time.delta_secs(),
        },
    );
}

/// `Update`: drain the **canonical** [`WindowEvent`] stream into `input`
/// records, in arrival order. This is the one stream winit fans out from (it
/// also writes the six typed channels — `CursorMoved`, `KeyboardInput`, … — but
/// those are derived shadows that scramble cross-channel order, and UI picking
/// reads only this combined stream). Recording here keeps the true move→click
/// interleaving a click depends on, and means replay can re-derive everything
/// downstream. Replay-irrelevant variants (lifecycle, IME, file-drop, window
/// move/occlude/theme, gestures, mouse-motion) are skipped — see [`InputRecord`].
pub fn record_inputs(
    mut writer: ResMut<TraceWriter>,
    frame: Res<FrameCount>,
    mut events: MessageReader<WindowEvent>,
) {
    let f = frame.0;
    for ev in events.read() {
        let Some(record) = to_input_record(ev) else {
            continue;
        };
        writer.write(f, Body::Input(record));
    }
}

/// Map one [`WindowEvent`] to its [`InputRecord`], or `None` for a variant
/// replay doesn't reproduce. Window entities are deliberately dropped (they
/// don't survive the trace boundary; the driver remaps to `PrimaryWindow`).
fn to_input_record(ev: &WindowEvent) -> Option<InputRecord> {
    Some(match ev {
        WindowEvent::KeyboardInput(e) => InputRecord::Key {
            key: format!("{:?}", e.key_code),
            state: button_state(e.state),
            repeat: e.repeat,
        },
        WindowEvent::KeyboardFocusLost(_) => InputRecord::KeyboardFocusLost,
        WindowEvent::MouseButtonInput(e) => InputRecord::MouseButton {
            button: format!("{:?}", e.button),
            state: button_state(e.state),
        },
        WindowEvent::CursorMoved(e) => InputRecord::Cursor {
            pos: [e.position.x, e.position.y],
        },
        WindowEvent::CursorEntered(_) => InputRecord::CursorEntered,
        WindowEvent::CursorLeft(_) => InputRecord::CursorLeft,
        WindowEvent::MouseWheel(e) => InputRecord::Wheel {
            unit: format!("{:?}", e.unit),
            x: e.x,
            y: e.y,
        },
        WindowEvent::TouchInput(e) => InputRecord::Touch {
            phase: format!("{:?}", e.phase),
            pos: [e.position.x, e.position.y],
            id: e.id,
        },
        WindowEvent::WindowResized(e) => InputRecord::Resize {
            size: [e.width, e.height],
        },
        WindowEvent::WindowScaleFactorChanged(e) => InputRecord::ScaleFactor {
            scale_factor: e.scale_factor,
        },
        _ => return None,
    })
}

fn button_state(state: bevy::input::ButtonState) -> &'static str {
    match state {
        bevy::input::ButtonState::Pressed => "pressed",
        bevy::input::ButtonState::Released => "released",
    }
}

/// `Update`: F10 → one `mark` record with the incremented counter. Reserved
/// (deferred) field: a screenshot path.
pub fn record_marks(
    mut writer: ResMut<TraceWriter>,
    frame: Res<FrameCount>,
    keys: Res<ButtonInput<KeyCode>>,
    mut counter: ResMut<MarkerCounter>,
) {
    if keys.just_pressed(KeyCode::F10) {
        counter.0 += 1;
        writer.write(frame.0, Body::Mark { marker: counter.0 });
    }
}

/// `Update`, after `coach::drain_events`: empty the decorator buffer into one
/// `coach` record (if any read was non-empty) and one `cmd` record per command
/// sent this frame. The buffer fills during `drain_events` (events/features)
/// and during any system that sent a command earlier this frame.
pub fn record_coach(
    mut writer: ResMut<TraceWriter>,
    frame: Res<FrameCount>,
    buffer: NonSend<TraceBufferHandle>,
) {
    let taken = buffer.borrow_mut().take();
    if taken.is_quiet() {
        return;
    }
    let f = frame.0;
    let read = CoachRead {
        events: taken.events,
        latest: taken.latest,
        drained: taken.drained,
    };
    if !read.is_empty() {
        writer.write(f, Body::Coach(read));
    }
    for cmd in taken.commands {
        writer.write(f, Body::Cmd { command: cmd });
    }
}

/// `Update`: record `AppState` transitions. Reads the transition messages so a
/// from→to pair is captured exactly when it happens.
pub fn record_state(
    mut writer: ResMut<TraceWriter>,
    frame: Res<FrameCount>,
    mut transitions: MessageReader<StateTransitionEvent<AppState>>,
) {
    for ev in transitions.read() {
        // `exited`/`entered` are `Option`; the very first transition into the
        // initial state has `exited: None`. Render both honestly.
        writer.write(
            frame.0,
            Body::State {
                from: opt_state(ev.exited),
                to: opt_state(ev.entered),
            },
        );
    }
}

fn opt_state(s: Option<AppState>) -> String {
    s.map_or_else(|| "—".to_string(), |s| format!("{s:?}"))
}

/// `PostUpdate` after `UiSystems::PostLayout`: the on-change `geom` pass.
///
/// Captures where nodes *landed* (the channel headless tests are blind to).
/// Writes a record only for entities whose recorded fields changed since last
/// frame, and a `gone: true` record for entities that vanished. Keyed by
/// widget path, never `Entity`.
pub fn record_geom(
    mut writer: ResMut<TraceWriter>,
    frame: Res<FrameCount>,
    mut memory: ResMut<GeomMemory>,
    nodes: Query<(
        Entity,
        &ComputedNode,
        &UiGlobalTransform,
        Option<&CalculatedClip>,
    )>,
    tree: PathTree,
) {
    let f = frame.0;
    let mut seen: HashMap<String, GeomSeen> = HashMap::with_capacity(memory.0.len());

    for (entity, node, xform, clip) in nodes.iter() {
        let geom = geom_fields(node, xform, clip);
        let hash = geom.hash();
        let path = widget_path(entity, &tree);
        let changed = memory.0.get(&path).map(|s| s.hash) != Some(hash);
        if changed {
            writer.write(f, Body::Geom(geom.into_record(path.clone(), entity)));
        }
        // Last writer wins if two live entities share a path this frame (a
        // degenerate case the sibling index is meant to prevent); the record
        // above still went out, so no geometry is lost.
        seen.insert(
            path,
            GeomSeen {
                hash,
                entity: entity.to_bits(),
            },
        );
    }

    // Vanish detection: any path present last frame and absent now. A path that
    // a respawned entity reoccupies is *not* vanished — it's in `seen`.
    for (path, prev) in memory.0.iter() {
        if !seen.contains_key(path) {
            writer.write(
                f,
                Body::Geom(GeomRecord {
                    path: path.clone(),
                    entity: prev.entity,
                    size_px: None,
                    rect_px: None,
                    clip_px: None,
                    rot: None,
                    scale_factor: None,
                    gone: true,
                }),
            );
        }
    }

    memory.0 = seen;
}

/// `PostUpdate` after `UiSystems::PostLayout`: write one `poly` record per
/// trace segment populated this frame by `apply_trace`. Runs in `PostUpdate`
/// (after `Update` where `apply_trace` runs), so the resource is always
/// current. Written unconditionally when the resource is non-empty — the
/// trace scrolls every frame, so segments change every frame anyway.
pub fn record_poly(
    mut writer: ResMut<TraceWriter>,
    frame: Res<FrameCount>,
    last_geom: Option<Res<crate::widgets::time_graph::systems::LastTraceGeom>>,
    lane_size: Option<Res<crate::widgets::time_graph::scene::TimeGraphPitchLaneSize>>,
) {
    let Some(last_geom) = last_geom else {
        return;
    };
    let f = frame.0;
    let logical_size = lane_size
        .as_ref()
        .and_then(|ls| ls.0)
        .map(|ls| ls.get())
        .unwrap_or(Vec2::ZERO);
    for snapshot in last_geom.0.iter() {
        let point_count = snapshot.lane_logical.len();
        writer.write(
            f,
            Body::Poly(PolyRecord {
                path: "time_graph/pitch_lane/trace".to_string(),
                lane_logical: snapshot.lane_logical.clone(),
                point_count,
                lane_size: [logical_size.x, logical_size.y],
                aabb_px: snapshot.aabb_px,
                clipped_aabb_px: snapshot.clipped_aabb_px,
                scale_factor: snapshot.scale_factor,
            }),
        );
    }
}

/// `Last`: flush the buffered writer so a crash next frame keeps every line.
pub fn flush_writer(mut writer: ResMut<TraceWriter>) {
    writer.flush();
}

/// `Last`: on a graceful exit (`AppExit` — window close / Cmd-Q), finish the
/// gzip stream so the trace carries a valid trailer and stock `gzcat`/`gunzip`
/// read it cleanly. Runs after [`flush_writer`]; finishing also flushes, and
/// the writer goes inert afterward (subsequent frames don't run on exit). A
/// hard crash skips this and leaves a recoverable trailerless stream.
pub fn finish_writer(
    mut writer: ResMut<TraceWriter>,
    mut exits: MessageReader<bevy::app::AppExit>,
) {
    if exits.read().next().is_some() {
        writer.finish();
    }
}

/// The recorded geometry of one node, pre-hash. Physical pixels throughout.
struct GeomFields {
    size: Vec2,
    /// Global axis-aligned rect (rotation-aware), physical px.
    rect: [f32; 4],
    clip: Option<[f32; 4]>,
    angle: f32,
    scale_factor: f32,
}

impl GeomFields {
    /// Quantize the float fields to a stable integer hash so sub-pixel jitter
    /// below the recorded precision doesn't spam change records. 0.1px grid.
    fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let q = |v: f32| (v * 10.0).round() as i64;
        q(self.size.x).hash(&mut h);
        q(self.size.y).hash(&mut h);
        for v in self.rect {
            q(v).hash(&mut h);
        }
        match self.clip {
            Some(c) => {
                1u8.hash(&mut h);
                for v in c {
                    q(v).hash(&mut h);
                }
            }
            None => 0u8.hash(&mut h),
        }
        q(self.angle).hash(&mut h);
        q(self.scale_factor).hash(&mut h);
        h.finish()
    }

    fn into_record(self, path: String, entity: Entity) -> GeomRecord {
        GeomRecord {
            path,
            entity: entity.to_bits(),
            size_px: Some([self.size.x, self.size.y]),
            rect_px: Some(self.rect),
            clip_px: self.clip,
            rot: (self.angle != 0.0).then_some(self.angle),
            scale_factor: Some(self.scale_factor),
            gone: false,
        }
    }
}

fn geom_fields(
    node: &ComputedNode,
    xform: &UiGlobalTransform,
    clip: Option<&CalculatedClip>,
) -> GeomFields {
    let size = node.size;
    let (_scale, angle, _translation) = xform.to_scale_angle_translation();
    // Global AABB of the (possibly rotated) box: transform the four local
    // corners and take their bounding box. Mirrors the layout test's
    // GlobalRect::from_node, so a reader compares apples to apples.
    let half = size * 0.5;
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
    GeomFields {
        size,
        rect: [min.x, min.y, max.x, max.y],
        clip: clip.map(|c| [c.clip.min.x, c.clip.min.y, c.clip.max.x, c.clip.max.y]),
        angle,
        scale_factor: if node.inverse_scale_factor > 0.0 {
            1.0 / node.inverse_scale_factor
        } else {
            1.0
        },
    }
}

/// Read-only view of the node hierarchy for [`widget_path`]: each node's
/// optional `Name`, its parent link, and (on a parent) the ordered children.
#[derive(bevy::ecs::system::SystemParam)]
pub struct PathTree<'w, 's> {
    nodes: Query<'w, 's, (Option<&'static Name>, Option<&'static ChildOf>)>,
    children: Query<'w, 's, &'static Children>,
}

/// Build a node's widget path: `Name` ancestry root→leaf joined with `/`, with
/// a `.<sibling-index>` suffix wherever a `Name` alone can't pin a node down —
/// a nameless node, or one of several same-named siblings (e.g.
/// `time_graph/lane/trace_layer/body.3`).
///
/// The sibling index is the node's ordinal in its parent's `Children`, which is
/// spawn order: deterministic, and stable run-to-run (and replay-to-replay) as
/// long as the widget spawns its children in the same order. *Entity* ids are
/// never used — a respawned node reuses an index with a new generation, so an
/// entity-derived segment would alias across repaints and break the diff the
/// trace exists for. Named, unique-among-siblings nodes keep a bare name; the
/// ordinal only appears where it must, so common paths stay readable.
fn widget_path(entity: Entity, tree: &PathTree) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut cur = entity;
    // Bound the walk defensively; UI trees are shallow.
    for _ in 0..64 {
        let (name, child_of) = match tree.nodes.get(cur) {
            Ok(v) => v,
            Err(_) => break,
        };
        let parent = child_of.map(|c| c.parent());
        segments.push(path_segment(cur, name, parent, tree));
        match parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    segments.reverse();
    segments.join("/")
}

/// One path segment for `cur`. A node whose `Name` is unique among its siblings
/// (or which is a root) uses the bare name. A nameless node, or one of several
/// siblings sharing a name, gets a `.<ordinal>` suffix where the ordinal is its
/// position in the parent's `Children` — the disambiguator that makes the path
/// unique within the frame and stable across runs.
fn path_segment(
    cur: Entity,
    name: Option<&Name>,
    parent: Option<Entity>,
    tree: &PathTree,
) -> String {
    let siblings = parent.and_then(|p| tree.children.get(p).ok());
    let ordinal = siblings.and_then(|c| c.iter().position(|e| e == cur));
    match (name, siblings, ordinal) {
        // Named and provably unique among siblings → bare name.
        (Some(n), Some(sibs), _) if !name_repeats(n, sibs, &tree.nodes) => n.as_str().to_string(),
        // Named but shares the name with a sibling → name + ordinal.
        (Some(n), _, Some(i)) => format!("{}.{i}", n.as_str()),
        // Named root (no parent / no Children) → bare name.
        (Some(n), _, _) => n.as_str().to_string(),
        // Nameless with a known slot → ordinal segment.
        (None, _, Some(i)) => format!(".{i}"),
        // Nameless root or detached → no stable handle; mark it as such.
        (None, _, None) => ".?".to_string(),
    }
}

/// Whether `name` is shared by more than one of `siblings` — i.e. the bare name
/// would be ambiguous and needs an ordinal.
fn name_repeats(
    name: &Name,
    siblings: &Children,
    nodes: &Query<(Option<&Name>, Option<&ChildOf>)>,
) -> bool {
    siblings
        .iter()
        .filter(|&e| matches!(nodes.get(e), Ok((Some(n), _)) if n == name))
        .nth(1)
        .is_some()
}
