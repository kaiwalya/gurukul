# `coach-game` architecture

*What is where, and why.* For how to **build** a widget, see
[`CONTRIBUTING.md`](CONTRIBUTING.md). For Bevy mechanics and local app
conventions, see [`AGENTS.md`](AGENTS.md).

## The unit is the widget slice

InGame UI is organised by **widget**, not by layer. Each widget is a vertical
slice under `widgets/<name>/` that owns the whole path from a domain fact to
pixels:

```
widgets/<name>/
  model.rs    domain → geometry projection (the only music-aware layer)
  scene.rs    render-facing contract (read model)
  systems.rs  Bevy node spawning, markers, painting, layout capture
  mod.rs      re-exports the slice (flat + explicit: pub use scene::{..}, model::{..})
game/<name>.rs  route glue: read app resources, write the scene, send commands
```

There are no top-level `models/` or `scenes/` directories — the layers are a
property of each slice's internals, not a crate-wide taxonomy.

## The four layers

A widget slice is **domain-aware as a whole**, but that awareness is
quarantined to one layer:

- **`model.rs`** — the *only* layer that knows music. A projection: it takes
  domain facts (scales, tonics, frequencies, features) and projects them into
  geometry and primitives (angles, normalized coordinates, slot states,
  strings). Plain Rust, no Bevy, no `bevy::Color`. After the model runs, music
  has been spent.
- **`scene.rs`** — the render-facing *contract* (a read model). Already-projected
  data only: angles and coordinates, never frequencies or raga names.
  Music-blind.
- **`systems.rs`** — Bevy-only presentation. Spawns the node tree (tagging
  nodes with marker components), reads the scene, and paints. Knows the engine,
  not the domain.
- **`game/<name>.rs`** — route glue, and the *only* place the two worlds touch.
  Reads app/domain resources, calls the widget `model`, writes the scene, and
  sends coach commands.

The dependency arrow points **inward**: glue depends on the widget; the widget
never depends on glue.

> A widget slice owns the whole path from domain snapshot to pixels; the model
> layer is where music becomes geometry, and nothing below it may know what a
> note is.

This is the governing doctrine. An earlier convention held that "widgets are
dumb geometry and callers translate the domain" — the music-blindness rule did
not die, it moved down one level to the `model`/`scene` seam.

## Reuse

The unit of reuse is the whole slice. A second route that wants the same widget
imports the widget module; nothing about the layout changes. A second consumer
that needs *different* domain semantics reuses the slice's `scene` + `systems`
(both music-blind) and brings its own model and glue:

> **Reuse the slice; bring your own model if your semantics differ.**

## Scene contracts are not one shape

The scene layer is whatever render-facing contract fits the widget. Two shapes
are in use, and that is deliberate — do not force uniformity:

- a **Resource** (e.g. `TimeGraphSceneRes`, `HudSceneRes`) when the widget is a
  singleton fed by a projection, or
- **components on the widget entity** (e.g. `DialScale`, `DialState`) when glue
  and widget systems meet on the entity and want change-detection to repaint
  exactly the part that changed.

## Marker ownership

Marker components live in the `systems.rs` of the widget that **owns the ECS
nodes**. A widget exposes its own `spawn(commands, parent)`; glue calls it and
applies route-level concerns (e.g. overlay positioning). Markers therefore stay
with the nodes they tag, and a widget's tree shape is assertable in a headless
test against the widget alone. Marker names carry no route vocabulary — a dial
root is `NoteDialRoot`, not `InGameDial`.

### Child ownership: a repaint system despawns only what it spawned

Marker ownership has a corollary for *destruction*. A system that clears and
rebuilds its children each frame must despawn **by its own marker query**, not
by clearing the parent wholesale: `despawn_related::<Children>()` on a parent
destroys *every* child, including children another system spawned there.

> **A repaint system may only despawn entities it spawned** — repaint via
> `Query<Entity, With<MyMarker>>`, never by clearing a parent you share.
> *Lifecycle teardown is exempt:* despawning a root or `DespawnOnExit(state)`
> recursively destroys a subtree you own, which is not the same as clearing a
> container you share.

The hazard is invisible to per-system tests by construction: each system is
correct alone; the defect only appears when two write to the same parent. The
marker-scoped rule is the *fallback* — the stronger fix is structural: don't
share the parent (see the viewport pattern below, where gridlines and trace are
separate layered children).

## A domain decision is made in domain units

Music-blindness has a mirror image, and it is a **universal rule for every
component**, not just the slices. The model rule says music flows *down* into
geometry and nothing below knows a note. This rule governs the other direction:

> **A *domain* decision is made in domain units.** What to draw, whether a point
> is in range, how far to zoom — decided in the domain (notes, `PitchLog2`, ms),
> never in pixel space. Pixels cross the seam in *both* directions, but the
> decision lives on the domain side: output pixels are computed *from* domain
> decisions; **input pixels (a click, a pinch) are converted to domain units at
> the boundary, and the decision is then made in the domain.**

A keep/drop decision made in *normalized graphical space* (clamping an
out-of-range point to the edge instead of dropping it in the domain) piles stale
points at the boundary — that is the escape this rule forbids. The line is
*domain* decision, not *all use of pixels*: intrinsically presentational
mechanics legitimately consume pixels and have no domain unit — scroll clamping
(`src/ui.rs` computes max-offset from a measured size), hit-test inversion,
overflow culling. Those are not domain decisions. The test is: would the answer
change if the same data were drawn at a different zoom or on a different display?
If yes, decide it in the domain.

**Corollary — unit-of-measure is part of the contract.** A bare `f32`/`Vec2`
carries no unit, so any value crossing the measure→paint seam must name its
frame. The concrete trap (`ComputedNode::size()` is *physical* pixels while
`px(...)` is *logical* — a 2× display doubles every coordinate) and the API that
converts it live in [`AGENTS.md`](AGENTS.md) with the other Bevy mechanics. The
*rule* is:

> A measured value crossing a frame boundary is converted at exactly one place,
> behind a frame-explicit newtype (e.g. `LogicalSize`) whose sole constructor
> performs the conversion — so a consumer **cannot** receive the wrong frame. A
> raw `Vec2` in a capture resource moves the disagreement from compile time to a
> line on the wrong side of the screen.

## Pattern: a parent owns the viewport (composite widgets only)

This is a **pattern, not a law** — reach for it only when a component has a
*shared coordinate system* that multiple children must agree on, plus a notion
of zoom/pan/extent. The time graph has both (gridlines and trace share one
pitch/time space); the dial and HUD do not. Do not invent a viewport parent to
satisfy a rule.

When it does apply, the parent owns a **domain-valued viewport** — the shared
coordinate system, expressed in domain units (e.g. pitch range `A2..A6` plus a
rolling time window). Layered children consume it:

```
Lane (parent)     owns: viewport { pitch range, rolling time window }  ← domain units
  ├─ Gridlines    (back z)   reads viewport; redraws when the viewport changes
  └─ Trace        (front z)  reads viewport; redraws when the data changes
```

Three properties follow, and each is the right-for-the-right-reason version of a
rule above:

- **One owner for the shared frame.** The domain→logical-pixel transform has a
  single home (parent holds viewport + measured logical size; children apply
  it), so the *domain-decisions* rule has one place to be right.
- **Separate layers, separate redraw triggers.** Gridlines are a function of the
  viewport (rare changes); the trace is a function of data (every frame). They
  are different things, so they are different children — which is *why* they
  don't despawn-fight, not merely *how* they avoid it.
- **Zoom is decided in the domain.** The viewport is chosen in domain units —
  set from outside ("show me A2–A6") *or* derived from the data's extent, per the
  widget's policy — so there is nothing to clamp in pixel space, and a
  fixed-range mode is just a different policy for the same field.

In the time graph this is `widgets/time_graph/`: the pitch lane holds two
full-size layer children, `GridlineLayer` (back z) and `TraceLayer` (front z),
each painted by its own system into its own parent — so the despawn-fight is
impossible by construction, not avoided by despawn-scoping. The "separate redraw
triggers" property is realized as two scene **resources** split by *cadence*, not
feature: `TimeGraphGridSceneRes` (grooves — slow) and `TimeGraphLiveSceneRes`
(trace + onset ticks + breath spans — fast, all normalized against the rolling
time window so they scroll every frame). The glue (`game/time_graph.rs`) projects
one scene and distributes it, value-gating the slow resource with `set_if_neq` so
a live-only frame leaves the gridlines untouched. The viewport itself is not a
separate ECS component — it is already domain-valued in the model's projector and
out-of-window points are dropped there, so an ECS mirror would be a second source
of truth with no readers.

### How children consume the viewport: cutout vs. project-in-window

A child reads the viewport in one of two ways, and which one depends on whether
its content is **fixed-extent or unbounded** — not on taste. This is the
familiar web split (a clip window sliding over an oversized child vs. a list that
only renders visible rows), applied per layer:

- **Cutout (fixed-extent content).** Draw the content for the current *zoom*
  once, in the child's own coordinates, and let the parent's clip show a window
  into it; *panning* is then just moving the window (`Overflow` + offset), no
  re-render. Right for **gridlines**: at a fixed zoom the set of lines is fixed,
  so panning time changes nothing and panning pitch just slides which lines show.
  Re-render only when the *zoom* changes.
- **Project-in-window (unbounded / live content).** The content grows without
  bound (a live trace adds points forever), so drawing it all and clipping would
  leak unbounded nodes behind the clip — the renderer still walks them. Instead
  the child uses the parent's domain window as a **cull**: project only the
  in-window points. Right for the **trace**. Note this is *why* culling must be a
  domain decision (the in/out test is in ms and `PitchLog2`, per the rule above),
  and it is what makes the DOM cutout model insufficient on its own here: a clip
  can pan a fixed child, but it cannot bound a child whose extent is infinite.

The viewport is what lets one parent serve both — same domain window, consumed
two ways: as a pan offset by the fixed layer, as a cull predicate by the live one.

## Testability follows the layers

The point of the seam is that each layer is verifiable on its own, so the
test levels map onto the layers. **Each level catches a different class of bug
and is blind to the others** — knowing what a level *cannot* see is as
important as knowing what it can, because a bug lives at exactly one layer and a
green test at the wrong layer is false confidence.

- **`model`** → pure tests. The projection is plain Rust, so its math is
  checked with no `App` at all.
  *Blind to:* whether the projection's **spec** is right. A pure test asserts
  the model matches what it was told to do; it cannot tell you that what it was
  told to do is correct. (This shipped a bug once: a normalize step *clamped*
  out-of-window points to the lane edge instead of *dropping* them — the test
  asserted the clamp, stayed green, and the clamp was the on-screen defect.)
- **`scene` + `systems`** → headless ECS tests. The spawned tree, its marker
  components, and the `Node` *inputs* are asserted against the widget alone
  (this is why markers live with the nodes — see above).
  *Blind to:* anything the layout engine computes. `MinimalPlugins` never runs
  `ui_layout_system`, so there is no `ComputedNode`, no global position, no
  measured size, no clipping. A headless test sees the `px`/`percent` you
  *requested*, never where the node *landed*.
- **layout-dependent geometry** → layout-aware tests, which run the *real*
  layout schedule and assert on **computed global coordinates** ("the trace
  body's global rect lies inside the lane's global rect"). This is the *only*
  level that can see the measure→paint seam, the physical/logical frame, and
  clipping — and only if it runs at a non-unit scale factor (at 1× the frame
  bug is mathematically invisible). The rules that keep this level honest —
  real producer never injected, assert existence not just shape, run at 2× —
  are *how-to* and live in [`CONTRIBUTING.md`](CONTRIBUTING.md).

A widget that is hard to test at one of these levels usually has a layer doing
another layer's job. The *how* of each level — the `tests/common` harness, the
layout-aware harness, the `ComputedNode` / `PostLayout` mechanics — belongs in
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Crate-level pieces (not in any slice)

- **`semantic_graph`** — the semantic pitch/time projection. It is an *upstream
  shared* read model: produced once and potentially consumed by several widgets
  (the time graph today; a spiral or score view later). It is not the time
  graph's private model, so it lives at crate level, not inside
  `widgets/time_graph/`. Its Bevy wrapper (`SemanticGraphRes`) lives in
  `game/mod.rs` so the projection module itself stays Bevy-free.
- **Out of slices on purpose**: `ui.rs` (shared button/colour primitives),
  `feature_history.rs`, `feature_types.rs`, `coach.rs`, `state.rs`, and
  everything under `menu/`.
