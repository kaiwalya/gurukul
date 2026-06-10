# Contributing to `coach-game`

*How to build a widget.* For **what** the layers are and **why** the crate is
shaped this way (the slice doctrine, music quarantine, scene shapes, marker
ownership), see [`ARCHITECTURE.md`](ARCHITECTURE.md). The backport plan for the
existing InGame UI lives in
[`../../docs/COACH_GAME_LAYERING_PLAN.md`](../../docs/COACH_GAME_LAYERING_PLAN.md).

Build UI in isolated pieces before stitching it into the screen. A widget
should be useful and testable through its own model, scene contract, and ECS
systems before the app route, menu, or game surface depends on it. The unit is
not a React-style function component — it is the vertical slice described in
ARCHITECTURE.md.

## Preferred build order

Build the slice bottom-up:

1. **Model** — the pure domain → geometry projection.
2. **Scene** — the render-facing contract the widget consumes.
3. **Systems** — Bevy code that spawns nodes and paints from the scene.

Example:

- `semantic_graph` computes semantic pitch/time geometry.
- `widgets::time_graph::TimeGraphSceneRes` is the render contract.
- `widgets::time_graph::*` systems build and update the UI tree.

## Testing ladder

The three test levels map onto the three layers (see
[`ARCHITECTURE.md`](ARCHITECTURE.md) for why); this section is how to write
each. Use all three for each non-trivial widget:

1. **Pure tests**
   Test the math or projection without an `App`.

2. **Headless ECS tests**
   Use the existing `tests/common` harness.
   Assert resources, entity counts, marker components, and parent/child structure.

3. **Layout-aware tests**
   When geometry depends on UI layout, capture `ComputedNode` after `UiSystems::PostLayout` and feed the next frame from that measured size.

This is the important Bevy-specific rule: layout-dependent UI often needs
a two-step loop. The Bevy scheduling mechanics for that loop live in
[`AGENTS.md`](AGENTS.md); this file owns the widget workflow.

- `Update`: build the scene and UI from last known layout inputs.
- `PostLayout`: capture measured sizes for the next frame.

Do not try to spawn layout-dependent nodes for the first time in `PostLayout` and expect them to be fully laid out immediately.

## Practical workflow

When building a new widget:

1. Write or adjust the pure model.
2. Add the scene/resource contract.
3. Spawn the minimal tree with marker components.
4. Add a headless test that proves the tree shape.
5. Add layout-dependent geometry only after the static tree is correct.
6. Capture measured layout into a resource if the geometry depends on actual lane/panel size.
7. Only then add styling and richer behaviour.
8. Stitch the widget into the app screen after the isolated model, scene, and widget tests pass.

This keeps iteration local and makes it possible to debug tree shape, layout, and rendering separately instead of all at once.
