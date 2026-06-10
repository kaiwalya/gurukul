# Contributing to `coach-game`

Build UI in isolated pieces before stitching it into the screen. A widget
should be useful and testable through its own model, scene contract, and
ECS systems before the app route, menu, or game surface depends on it.

This app is easiest to build one widget at a time, but the unit is not a
React-style function component. In Bevy, the stable unit is:

1. a pure model or projection step,
2. a small scene/resource contract,
3. a widget system pair that turns that contract into ECS nodes.

## Preferred build order

For new UI work, split the job into three layers:

1. **Model**
   Plain Rust, no Bevy. Keep music/domain logic here.

2. **Scene**
   A small render-facing struct or resource. This is the widget input.

3. **Widget**
   Bevy-only code that reads the scene and owns nodes, transforms, colours, and scheduling.

Example:

- `graph_model` computes semantic pitch/time geometry.
- `widgets::time_graph::TimeGraphSceneRes` is the render contract.
- `widgets::time_graph::*` systems build and update the UI tree.

## Testing ladder

Use the same three test levels for each non-trivial widget:

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

## Widget rules

Keep widgets narrow:

- Widgets should not own business logic.
- Widgets should have a small number of input resources/components.
- Important nodes should have marker components so tests can query them directly.

For example, a graph widget should expose markers like:

- root
- lanes
- repeated visual children such as ticks, spans, or trace bodies

That gives tests a stable surface even when the visual tree grows.

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
