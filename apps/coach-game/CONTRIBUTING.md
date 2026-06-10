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

The test levels map onto the layers (see [`ARCHITECTURE.md`](ARCHITECTURE.md)
for why, and for what each level is **blind to**); this section is how to write
each. Use the levels a widget actually needs — a widget whose geometry depends
on measured layout needs all three; a static one may stop at two.

1. **Pure tests**
   Test the math or projection without an `App`.
   Remember this level only proves the model matches its spec, not that the
   spec is right — when a projection has a policy choice (clamp vs. drop an
   out-of-range point, bridge vs. split a gap), test the *consequence* you
   actually want on screen, not merely that the code does what it says.

2. **Headless ECS tests**
   Use the existing `tests/common` harness (`MinimalPlugins`, `FakeCoach`).
   Assert resources, entity counts, marker components, and parent/child
   structure. This level cannot see layout — there is no `ComputedNode` under
   `MinimalPlugins` — so never assert a *measured* size or a global position
   here, and never hand-inject one to fake it (see level 3).

3. **Layout-aware tests**
   The only level that exercises the measure→paint seam. Required for any
   widget whose painted geometry is computed from a captured layout size.

   *Harness:* `tests/common::layout_app()` — to be added with the first
   layer-3 test. Until it exists, that first test **is** the spec for the
   harness; build it to satisfy the rules below, then extract the helper.

   - **Run the real layout schedule** so `ComputedNode` is populated — not
     `MinimalPlugins`. Drive enough frames for the capture→paint loop to settle
     (capture runs after `UiSystems::PostLayout`; paint reads it the next frame
     — see [`AGENTS.md`](AGENTS.md) for the schedule mechanics).
   - **Run at scale factor 2.0.** This is the easiest rule to forget and the
     most important. At 1× physical and logical pixels coincide, so a
     physical/logical frame bug passes every assertion — the test certifies the
     broken code. Set the test window's scale factor (or `UiScale`) to 2.0.
   - **Assert on computed global coordinates**, e.g. every painted body's
     global rect lies within its lane's global rect. That assertion — *is it on
     screen where it should be* — is the whole point; `Node` input fields
     (level 2) cannot express it.
   - **Real producer, never injected.** Let the real capture system produce the
     measured size. A hand-picked `Vec2(200, 100)` tests your guess about the
     size, not the system that produces it — and the seam bug lives precisely
     in that producer (the physical/logical frame — see *The physical/logical
     pixel trap* below, and [`AGENTS.md`](AGENTS.md) for the API).
   - **Assert existence, not just placement.** "Given features in, the bodies
     exist" is a distinct check that catches the worst failure: a paint system
     that never runs in the suite at all.
   - **Migrate, don't fake, repaint-skip coverage.** The early-return that skips
     repaint when nothing changed is real behaviour worth a test — assert entity
     IDs are *stable* across a no-change frame, at this level with the real
     producer. Do **not** keep it alive with an injected size or a `#[cfg(test)]`
     constructor on the frame newtype; that reopens the exact hole the newtype
     closes.

   Do not try to spawn layout-dependent nodes for the first time in
   `PostLayout` and expect them fully laid out immediately.

### The physical/logical pixel trap

Convert a captured size at capture time, behind the frame newtype — trap and API
in [`AGENTS.md`](AGENTS.md), rule in [`ARCHITECTURE.md`](ARCHITECTURE.md). Never
store a raw `Vec2` size in a capture resource.

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
