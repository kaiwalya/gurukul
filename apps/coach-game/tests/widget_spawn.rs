//! Pure widget-spawn tests: each widget's `spawn` is run against a bare
//! `World` (no `build_app`, no route glue, no schedule), and its tree shape
//! is asserted against the widget alone. This is the literal form of the
//! "a widget's tree is assertable in isolation" guarantee that marker
//! ownership exists to provide — if a widget breaks its own tree, these
//! fail without dragging the whole app in.
//!
//! Also covers the time-graph `apply_gridlines` / `apply_events`
//! change-detection guards, driven by a minimal `Schedule` over a `World`.
//! The trace painter (`apply_trace`) is *not* tested here: it consumes a
//! measured lane size, so faking one with an injected `Vec2` would certify
//! the very physical/logical frame bug the `LogicalSize` newtype exists to
//! prevent. Its paint/skip behaviour is covered at layer 3 with the real
//! producer — see `tests/time_graph_layout.rs` and the layer-3 rules in
//! `CONTRIBUTING.md`.

use bevy::ecs::world::CommandQueue;
use bevy::prelude::*;

use coach_game::game::time_graph::refresh_scene;
use coach_game::game::SemanticGraphRes;
use coach_game::semantic_graph::{
    GrooveLine, PitchWindow, SemanticGraph, TimeWindow, TracePoint, TraceSegment,
};
use coach_game::widgets::hud::{self, HudBadge, HudDegRow};
use coach_game::widgets::note_dial::{
    self, DialHub, DialHubLabel, DialScale, DialState, NoteDialRoot,
};
use coach_game::widgets::scale_picker::{
    self, PickerRow, ScalePickerCloseButton, ScalePickerRoot, ScalePickerRows, ScaleRow,
};
use coach_game::widgets::time_graph::{
    self, systems as tg, GridlineLayer, GrooveLineMarker, NormalizedGrooveLine,
    NormalizedOnsetTick, OnsetTickMarker, TimeGraphEventsLane, TimeGraphGridSceneRes,
    TimeGraphLiveSceneRes, TimeGraphPitchLane, TimeGraphRoot, TraceLayer, TraceSegmentBody,
};
use domain_ports::pitch::PitchLog2;

/// A throwaway marker so a test can recover the entity `spawn` returned.
#[derive(Component)]
struct Probe;

/// Run `f` (which queues spawn commands) against a fresh `World`, flush the
/// queue, and return the `World` plus the parent entity the widget was
/// spawned under.
fn spawn_world(f: impl FnOnce(&mut Commands, Entity)) -> (World, Entity) {
    let mut world = World::new();
    let parent = world.spawn_empty().id();
    let mut queue = CommandQueue::default();
    {
        let mut commands = Commands::new(&mut queue, &world);
        f(&mut commands, parent);
    }
    queue.apply(&mut world);
    (world, parent)
}

fn count<C: Component>(world: &mut World) -> usize {
    world
        .query_filtered::<Entity, With<C>>()
        .iter(world)
        .count()
}

/// Number of entities tagged `C` whose `ChildOf` points at `parent`.
fn children_with<C: Component>(world: &mut World, parent: Entity) -> usize {
    world
        .query_filtered::<&ChildOf, With<C>>()
        .iter(world)
        .filter(|c| c.parent() == parent)
        .count()
}

fn parent_of(world: &World, e: Entity) -> Entity {
    world.entity(e).get::<ChildOf>().unwrap().parent()
}

// --- note_dial --------------------------------------------------------

#[test]
fn note_dial_spawn_builds_shell_hub_and_label() {
    let (mut world, parent) = spawn_world(|commands, parent| {
        let shell = note_dial::spawn(commands, parent);
        commands.entity(shell).insert(Probe);
    });

    assert_eq!(count::<NoteDialRoot>(&mut world), 1, "one shell");
    assert_eq!(count::<DialHub>(&mut world), 1, "one hub");
    assert_eq!(count::<DialHubLabel>(&mut world), 1, "one hub label");

    // The returned id is the shell, and the shell carries the scene contract.
    let shell = world
        .query_filtered::<Entity, (
            With<Probe>,
            With<NoteDialRoot>,
            With<DialScale>,
            With<DialState>,
        )>()
        .single(&world)
        .expect("returned id is the shell carrying DialScale + DialState");
    assert_eq!(parent_of(&world, shell), parent, "shell under given parent");
    // The shell spawns empty — no slots until glue fills them.
    assert!(
        world
            .entity(shell)
            .get::<DialScale>()
            .unwrap()
            .slots
            .is_empty(),
        "shell spawns with no slots"
    );
}

// --- hud --------------------------------------------------------------

#[test]
fn hud_spawn_builds_badge_with_row_child() {
    let (mut world, parent) = spawn_world(|commands, parent| {
        let badge = hud::spawn(commands, parent);
        commands.entity(badge).insert(Probe);
    });

    assert_eq!(count::<HudBadge>(&mut world), 1, "one badge");
    assert_eq!(count::<HudDegRow>(&mut world), 1, "one row");

    let badge = world
        .query_filtered::<Entity, (With<Probe>, With<HudBadge>)>()
        .single(&world)
        .expect("returned id is the badge");
    assert_eq!(parent_of(&world, badge), parent, "badge under given parent");
    assert_eq!(
        children_with::<HudDegRow>(&mut world, badge),
        1,
        "row under badge"
    );
}

// --- scale_picker -----------------------------------------------------

#[test]
fn scale_picker_spawn_builds_overlay_with_one_row_per_shape() {
    let rows = vec![
        PickerRow {
            label: "2 2 1 2 2 2 1".into(),
        },
        PickerRow {
            label: "2 1 2 2 1 2 2".into(),
        },
        PickerRow {
            label: "2 2 2 1 2 2 1".into(),
        },
    ];
    let rows_len = rows.len();
    let (mut world, parent) = spawn_world(|commands, parent| {
        let root = scale_picker::spawn(commands, parent, &rows);
        commands.entity(root).insert(Probe);
    });

    assert_eq!(count::<ScalePickerRoot>(&mut world), 1, "one overlay root");
    assert_eq!(
        count::<ScalePickerRows>(&mut world),
        1,
        "one rows container"
    );
    assert_eq!(
        count::<ScalePickerCloseButton>(&mut world),
        1,
        "one close btn"
    );
    assert_eq!(count::<ScaleRow>(&mut world), rows_len, "one row per shape");

    // Row markers carry catalogue indices 0..n, so a click maps to a shape.
    let mut indices: Vec<usize> = world
        .query::<&ScaleRow>()
        .iter(&world)
        .map(|r| r.0)
        .collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        (0..rows_len).collect::<Vec<_>>(),
        "rows indexed 0..n"
    );

    let root = world
        .query_filtered::<Entity, (With<Probe>, With<ScalePickerRoot>)>()
        .single(&world)
        .expect("returned id is the overlay root");
    assert_eq!(parent_of(&world, root), parent, "root under given parent");
}

#[test]
fn scale_picker_spawn_with_no_shapes_has_root_and_close_but_no_rows() {
    let (mut world, _parent) = spawn_world(|commands, parent| {
        scale_picker::spawn(commands, parent, &Vec::new());
    });
    assert_eq!(count::<ScalePickerRoot>(&mut world), 1);
    assert_eq!(
        count::<ScalePickerRows>(&mut world),
        1,
        "container still spawns"
    );
    assert_eq!(count::<ScalePickerCloseButton>(&mut world), 1);
    assert_eq!(
        count::<ScaleRow>(&mut world),
        0,
        "no rows for empty catalogue"
    );
}

// --- time_graph spawn -------------------------------------------------

#[test]
fn time_graph_spawn_builds_root_and_two_lanes() {
    let (mut world, parent) = spawn_world(|commands, parent| {
        let root = time_graph::spawn(commands, parent);
        commands.entity(root).insert(Probe);
    });

    assert_eq!(count::<TimeGraphRoot>(&mut world), 1, "one root");
    assert_eq!(count::<TimeGraphPitchLane>(&mut world), 1, "one pitch lane");
    assert_eq!(
        count::<TimeGraphEventsLane>(&mut world),
        1,
        "one events lane"
    );

    let root = world
        .query_filtered::<Entity, (With<Probe>, With<TimeGraphRoot>)>()
        .single(&world)
        .expect("returned id is the root");
    assert_eq!(parent_of(&world, root), parent, "root under given parent");
    assert_eq!(children_with::<TimeGraphPitchLane>(&mut world, root), 1);
    assert_eq!(children_with::<TimeGraphEventsLane>(&mut world, root), 1);

    // The pitch lane holds two stacked layer children — gridlines behind,
    // trace in front — so the two painters never share a parent (the
    // structural fix for the despawn-fight hazard).
    assert_eq!(count::<GridlineLayer>(&mut world), 1, "one gridline layer");
    assert_eq!(count::<TraceLayer>(&mut world), 1, "one trace layer");
    let pitch_lane = world
        .query_filtered::<Entity, With<TimeGraphPitchLane>>()
        .single(&world)
        .expect("pitch lane exists");
    assert_eq!(
        children_with::<GridlineLayer>(&mut world, pitch_lane),
        1,
        "gridline layer under the pitch lane"
    );
    assert_eq!(
        children_with::<TraceLayer>(&mut world, pitch_lane),
        1,
        "trace layer under the pitch lane"
    );
}

// --- time_graph apply_gridlines / apply_events change detection -------

fn one_groove() -> TimeGraphGridSceneRes {
    TimeGraphGridSceneRes {
        grooves: vec![NormalizedGrooveLine {
            y: 0.5,
            slot: 0,
            active: true,
        }],
    }
}

fn one_onset() -> TimeGraphLiveSceneRes {
    TimeGraphLiveSceneRes {
        pitch_segments: vec![],
        onset_ticks: vec![NormalizedOnsetTick {
            x: 0.25,
            strength: 0.9,
        }],
        breath_spans: vec![],
    }
}

/// A `World` with the time-graph tree spawned and both cadence-split scene
/// resources inserted, ready to run the painters via a `Schedule`.
fn world_with_graph(grid: TimeGraphGridSceneRes, live: TimeGraphLiveSceneRes) -> World {
    let (mut world, _parent) = spawn_world(|commands, parent| {
        time_graph::spawn(commands, parent);
    });
    world.insert_resource(grid);
    world.insert_resource(live);
    world
}

#[test]
fn apply_gridlines_paints_then_skips_when_unchanged() {
    let mut world = world_with_graph(one_groove(), one_onset());
    let mut schedule = Schedule::default();
    schedule.add_systems(tg::apply_gridlines);

    // First run: layer is empty → paint regardless of change state.
    schedule.run(&mut world);
    world.clear_trackers();
    assert_eq!(count::<GrooveLineMarker>(&mut world), 1, "groove painted");

    // Run again unchanged with grooves present: the guard short-circuits,
    // leaving the SAME marker entity — not despawn+respawn.
    let groove_before = world
        .query_filtered::<Entity, With<GrooveLineMarker>>()
        .single(&world)
        .unwrap();
    schedule.run(&mut world);
    world.clear_trackers();
    let groove_after = world
        .query_filtered::<Entity, With<GrooveLineMarker>>()
        .single(&world)
        .unwrap();
    assert_eq!(
        groove_before, groove_after,
        "unchanged grid scene must not despawn/respawn grooves"
    );
}

#[test]
fn apply_events_repaints_when_live_scene_changes() {
    let mut world = world_with_graph(one_groove(), one_onset());
    let mut schedule = Schedule::default();
    schedule.add_systems(tg::apply_events);

    schedule.run(&mut world);
    world.clear_trackers();
    assert_eq!(count::<OnsetTickMarker>(&mut world), 1);

    // Mutate the live scene → two onset ticks. ResMut access marks it changed.
    {
        let mut live = world.resource_mut::<TimeGraphLiveSceneRes>();
        live.onset_ticks = vec![
            NormalizedOnsetTick {
                x: 0.2,
                strength: 0.5,
            },
            NormalizedOnsetTick {
                x: 0.6,
                strength: 0.5,
            },
        ];
    }
    schedule.run(&mut world);
    world.clear_trackers();
    assert_eq!(
        count::<OnsetTickMarker>(&mut world),
        2,
        "repainted from new live scene"
    );
}

#[test]
fn apply_gridlines_does_not_despawn_trace_bodies() {
    // The defect-3 guard, now STRUCTURAL. Gridlines and trace live in
    // separate layer children of the pitch lane, so `apply_gridlines`
    // clears only the gridline layer — it cannot reach a trace body in the
    // trace layer. This passes by construction (separate parents), which is
    // the point of the refactor: the regression anchor is that the layers
    // stay separate. Runs `apply_gridlines` ALONE, with a trace body planted
    // in the trace layer, and asserts the body survives a groove repaint.
    let mut world = world_with_graph(one_groove(), one_onset());

    // Plant a trace body in the trace layer, as `apply_trace` would.
    let trace_layer = world
        .query_filtered::<Entity, With<TraceLayer>>()
        .single(&world)
        .expect("trace layer exists");
    let trace_body = world.spawn((TraceSegmentBody, ChildOf(trace_layer))).id();

    let mut schedule = Schedule::default();
    schedule.add_systems(tg::apply_gridlines);

    // First run paints grooves into the gridline layer (it was empty).
    schedule.run(&mut world);
    world.clear_trackers();
    assert!(
        count::<GrooveLineMarker>(&mut world) >= 1,
        "grooves painted"
    );
    assert!(
        world.get_entity(trace_body).is_ok(),
        "trace body must survive the first groove paint"
    );

    // Change the grid scene so `apply_gridlines` despawns + repaints grooves.
    {
        let mut grid = world.resource_mut::<TimeGraphGridSceneRes>();
        grid.grooves[0].active = !grid.grooves[0].active;
    }
    schedule.run(&mut world);
    world.clear_trackers();
    assert!(
        world.get_entity(trace_body).is_ok(),
        "trace body in its own layer must survive an apply_gridlines repaint"
    );
}

// --- time_graph cadence split -----------------------------------------

/// A graph carrying one groove and a two-point trace, with the windows the
/// model needs to normalize both. `trace_t` lets a caller advance the trace
/// while leaving the grooves identical.
fn graph_with(trace_t: u64) -> SemanticGraph {
    SemanticGraph {
        time_window: Some(TimeWindow {
            start_ms: 0,
            end_ms: 100,
        }),
        pitch_window: Some(PitchWindow {
            min: PitchLog2(8.0),
            max: PitchLog2(10.0),
        }),
        trace_segments: vec![TraceSegment {
            points: vec![
                TracePoint {
                    t_ms: trace_t,
                    pitch: PitchLog2(8.5),
                    confidence: 0.5,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                },
                TracePoint {
                    t_ms: trace_t + 10,
                    pitch: PitchLog2(9.0),
                    confidence: 0.5,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                },
            ],
        }],
        grooves: vec![GrooveLine {
            pitch: PitchLog2(9.0),
            slot: 3,
            active: true,
        }],
        onset_ticks: vec![],
        breath_spans: vec![],
    }
}

#[test]
fn live_only_frame_does_not_respawn_gridlines() {
    // The property the cadence split exists to provide. The glue
    // (`refresh_scene`) projects one scene and distributes it into a slow
    // grid resource (value-gated with `set_if_neq`) and a fast live resource.
    // When only the trace moves — same grooves, new trace points — the grid
    // resource is NOT marked changed, so `apply_gridlines` skips and the
    // gridline entities keep their identities. Drive the REAL glue so this
    // tests the `set_if_neq` value-gate, not a hand-rolled stand-in.
    let (mut world, _parent) = spawn_world(|commands, parent| {
        time_graph::spawn(commands, parent);
    });
    world.insert_resource(SemanticGraphRes(graph_with(0)));
    world.init_resource::<TimeGraphGridSceneRes>();
    world.init_resource::<TimeGraphLiveSceneRes>();

    let mut schedule = Schedule::default();
    schedule.add_systems((refresh_scene, tg::apply_gridlines).chain());

    // First frame: grid resource is written, grooves paint into the layer.
    schedule.run(&mut world);
    world.clear_trackers();
    assert_eq!(count::<GrooveLineMarker>(&mut world), 1, "groove painted");
    let groove_before = world
        .query_filtered::<Entity, With<GrooveLineMarker>>()
        .single(&world)
        .unwrap();

    // Live-only change: advance the trace, leave the groove untouched.
    world.resource_mut::<SemanticGraphRes>().0 = graph_with(5);
    schedule.run(&mut world);
    world.clear_trackers();

    let groove_after = world
        .query_filtered::<Entity, With<GrooveLineMarker>>()
        .single(&world)
        .unwrap();
    assert_eq!(
        groove_before, groove_after,
        "a live-only frame must not respawn gridlines (set_if_neq value-gates the grid resource)"
    );
}
