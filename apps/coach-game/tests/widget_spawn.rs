//! Pure widget-spawn tests: each widget's `spawn` is run against a bare
//! `World` (no `build_app`, no route glue, no schedule), and its tree shape
//! is asserted against the widget alone. This is the literal form of the
//! "a widget's tree is assertable in isolation" guarantee that marker
//! ownership exists to provide — if a widget breaks its own tree, these
//! fail without dragging the whole app in.
//!
//! Also covers the time-graph `apply_scene` change-detection guard, driven
//! by a minimal `Schedule` over a `World`. The trace painter
//! (`apply_trace_scene`) is *not* tested here: it consumes a measured lane
//! size, so faking one with an injected `Vec2` would certify the very
//! physical/logical frame bug the `LogicalSize` newtype exists to prevent.
//! Its paint/skip behaviour is covered at layer 3 with the real producer —
//! see `tests/time_graph_layout.rs` and the layer-3 rules in
//! `CONTRIBUTING.md`.

use bevy::ecs::world::CommandQueue;
use bevy::prelude::*;

use coach_game::widgets::hud::{self, HudBadge, HudDegRow};
use coach_game::widgets::note_dial::{
    self, DialHub, DialHubLabel, DialScale, DialState, NoteDialRoot,
};
use coach_game::widgets::scale_picker::{
    self, PickerRow, ScalePickerCloseButton, ScalePickerRoot, ScalePickerRows, ScaleRow,
};
use coach_game::widgets::time_graph::{
    self, systems as tg, GrooveLineMarker, NormalizedGrooveLine, NormalizedOnsetTick,
    NormalizedPoint, NormalizedTracePoint, NormalizedTraceSegment, OnsetTickMarker,
    TimeGraphEventsLane, TimeGraphPitchLane, TimeGraphRoot, TimeGraphScene, TimeGraphSceneRes,
    TraceSegmentBody,
};

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
}

// --- time_graph apply_scene / apply_trace_scene change detection ------

fn scene_with_one_of_each() -> TimeGraphScene {
    TimeGraphScene {
        pitch_segments: vec![NormalizedTraceSegment {
            points: vec![
                NormalizedTracePoint {
                    point: NormalizedPoint { x: 0.1, y: 0.2 },
                    confidence: 0.5,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                },
                NormalizedTracePoint {
                    point: NormalizedPoint { x: 0.9, y: 0.8 },
                    confidence: 0.5,
                    vibrato_rate: 0.0,
                    vibrato_depth: 0.0,
                },
            ],
        }],
        grooves: vec![NormalizedGrooveLine {
            y: 0.5,
            slot: 0,
            active: true,
        }],
        onset_ticks: vec![NormalizedOnsetTick {
            x: 0.25,
            strength: 0.9,
        }],
        breath_spans: vec![],
    }
}

/// A `World` with the time-graph tree spawned and the scene resource
/// inserted, ready to run `apply_scene` via a `Schedule`.
fn world_with_graph(scene: TimeGraphScene) -> World {
    let (mut world, _parent) = spawn_world(|commands, parent| {
        time_graph::spawn(commands, parent);
    });
    world.insert_resource(TimeGraphSceneRes(scene));
    world
}

#[test]
fn apply_scene_paints_then_skips_when_unchanged() {
    let mut world = world_with_graph(scene_with_one_of_each());
    let mut schedule = Schedule::default();
    schedule.add_systems(tg::apply_scene);

    // First run: lanes are empty → paint regardless of change state.
    schedule.run(&mut world);
    world.clear_trackers();
    assert_eq!(count::<GrooveLineMarker>(&mut world), 1, "groove painted");
    assert_eq!(count::<OnsetTickMarker>(&mut world), 1, "onset painted");

    // Run again with the scene unchanged and the lanes non-empty: the guard
    // (`!is_changed() && both lanes non-empty`) must short-circuit, leaving
    // the SAME marker entities — not despawn+respawn.
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
        "unchanged scene must not despawn/respawn markers"
    );
}

#[test]
fn apply_scene_repaints_when_scene_changes() {
    let mut world = world_with_graph(scene_with_one_of_each());
    let mut schedule = Schedule::default();
    schedule.add_systems(tg::apply_scene);

    schedule.run(&mut world);
    world.clear_trackers();
    assert_eq!(count::<OnsetTickMarker>(&mut world), 1);

    // Mutate the scene → two onset ticks. ResMut access marks it changed.
    {
        let mut scene = world.resource_mut::<TimeGraphSceneRes>();
        scene.0.onset_ticks = vec![
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
        "repainted from new scene"
    );
}

#[test]
fn apply_scene_repaint_does_not_despawn_trace_bodies() {
    // The defect-3 guard. `apply_scene` and `apply_trace_scene` both parent
    // into the pitch lane. When `apply_scene` repaints its grooves it must
    // despawn only *its own* markers — not clear the lane wholesale, which
    // would also destroy the trace bodies `apply_trace_scene` owns. This
    // runs `apply_scene` ALONE (no `.chain()`ed trace repaint to mask the
    // damage), so the old `despawn_related::<Children>()` would fail it
    // while the production `.chain()` ordering hid the bug.
    let mut world = world_with_graph(scene_with_one_of_each());

    // Plant a trace body in the pitch lane, as `apply_trace_scene` would.
    let pitch_lane = world
        .query_filtered::<Entity, With<TimeGraphPitchLane>>()
        .single(&world)
        .expect("pitch lane exists");
    let trace_body = world.spawn((TraceSegmentBody, ChildOf(pitch_lane))).id();

    let mut schedule = Schedule::default();
    schedule.add_systems(tg::apply_scene);

    // First run paints grooves into the same lane (markers were empty).
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

    // Change the scene so `apply_scene` despawns + repaints its grooves.
    {
        let mut scene = world.resource_mut::<TimeGraphSceneRes>();
        scene.0.grooves[0].active = !scene.0.grooves[0].active;
    }
    schedule.run(&mut world);
    world.clear_trackers();
    assert!(
        world.get_entity(trace_body).is_ok(),
        "trace body must survive an apply_scene groove repaint (shared-parent despawn hazard)"
    );
}
