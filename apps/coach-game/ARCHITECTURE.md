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
  mod.rs      re-exports the slice (flat: pub use scene::*, model::*)
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

## Testability follows the layers

The point of the seam is that each layer is verifiable on its own, so the
three test levels map one-to-one onto the layers:

- **`model`** → pure tests. The projection is plain Rust, so its math is
  checked with no `App` at all.
- **`scene` + `systems`** → headless ECS tests. The spawned tree and its
  marker components are asserted against the widget alone (this is why markers
  live with the nodes — see above).
- **layout-dependent geometry** → layout-aware tests, which run the two-step
  capture-then-paint loop.

A widget that is hard to test at one of these levels usually has a layer doing
another layer's job. The *how* of each level — the `tests/common` harness, the
`ComputedNode` / `PostLayout` mechanics — lives in
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
