# Coach-game UI layering refactor plan

## Summary

Refactor `apps/coach-game` so the InGame UI visibly follows the widget
workflow in `apps/coach-game/CONTRIBUTING.md`. The unit of organisation
is the **widget**, not the layer: each widget is a vertical slice under
`widgets/<name>/` containing its three layers as files, and `game/`
stays the InGame route glue that stitches widgets to app state.

```
widgets/<name>/
  model.rs    pure Rust projection and domain decisions, no Bevy ECS types
  scene.rs    render-facing contract (resource or components) consumed by systems
  systems.rs  Bevy-only tree spawning, markers, layout capture, paint/update
  mod.rs      re-exports the slice
game/<name>.rs  route glue: read app resources, write scenes, send coach commands
```

The layers are a property of each widget's internals, not a crate-wide
taxonomy — there are no top-level `models/` or `scenes/` directories.
The unit of reuse is the whole slice: if a second route ever consumes a
widget, it imports the widget module; nothing about the layout changes.
If a second consumer needs *different* domain semantics, it reuses the
slice's `scene` + `systems` (both music-blind) and brings its own model
and glue. **Reuse the slice; bring your own model if your semantics differ.**

### What each layer is (the doctrine this refactor commits to)

A widget slice is **domain-aware as a whole**, but the awareness is
quarantined to one layer:

- `model.rs` — the only layer that knows music. It is a *projection*:
  it takes domain facts (scales, tonics, frequencies, features) and
  projects them into geometry and primitives (angles, normalized
  coordinates, slot states, strings). Pure Rust, no Bevy. After it runs,
  music has been spent.
- `scene.rs` — the render-facing *contract* (read model). Already-projected
  data only: angles and coordinates, never frequencies or raga names.
  Music-blind.
- `systems.rs` — Bevy-only presentation. Spawns the node tree (with
  marker components), reads the scene, paints. Knows the engine, not the
  domain.
- `game/<name>.rs` — the only place the two worlds touch. Reads app/domain
  resources, calls the widget `model`, writes the scene, sends coach commands.

This **redefines** the prior "widgets are dumb geometry; callers translate"
doctrine: the music-blindness rule does not die, it moves down one level to
the `model`/`scene` seam. `widgets/mod.rs` and the widget header doc comments
(e.g. `widgets/note_dial.rs:3-5`) currently state the old rule and must be
rewritten, not merely re-pointed.

This is a behavior-preserving refactor. The time graph becomes the
template, and dial, HUD, and scale picker are brought into the same shape.

**One name per widget, used at every layer**: `note_dial`, `hud`,
`scale_picker`, `time_graph`. No layer calls the dial "dial" while
another calls it "note_dial".

## Key changes

- Rename and keep the semantic graph projection at crate level:
  - Move `graph_model.rs` to `semantic_graph.rs`. (`SemanticGraphRes`, the
    Bevy wrapper, stays in `game/mod.rs` — the module remains Bevy-free.)
    It is the *semantic* pitch/time projection,
    upstream of the time-graph widget and potentially consumed by other
    widgets later — it is not the time graph's private model, so it does
    not move into the widget slice.

- Split time graph into its slice `widgets/time_graph/`:
  - `scene.rs`: `TimeGraphScene`, the normalized structs, `TimeGraphSceneRes`.
  - `model.rs`: `project_scene` and the pure normalization math.
  - `systems.rs`: Bevy UI nodes, marker components, layout capture, paint.
  - Keep `game/time_graph.rs` as the feeder from `SemanticGraphRes` to
    `TimeGraphSceneRes`.

- Split dial into `widgets/note_dial/`:
  - `model.rs`: slot angles (`tick_angle`), needle angles (`needle_angle`),
    scale-to-dial projection (`build_slots`), features-to-needle projection,
    capture-Sa scale calculation, and the hub three-state visual resolution.
    The hub resolver returns a plain state **enum** (e.g. hidden / disabled /
    enabled / pressed), never a `bevy::Color` — per CONTRIBUTING.md "plain
    Rust, no Bevy," the model stays Bevy-free and `systems.rs` maps the enum
    to colours.
  - `scene.rs`: `DialScale`, `DialSlot`, `DialState`, `Needle`, `NeedleStyle`.
  - `systems.rs`: ECS child nodes, marker components, slot/needle painting,
    current-slot geometry.
  - **Widget owns spawn; stays placement-agnostic.**
    `widgets::note_dial::systems` exposes `spawn(commands, parent) -> Entity`
    that builds the shell, hub, and hub label with their marker components and
    returns the shell entity — matching the time-graph template
    (`game/time_graph.rs` calls into the widget). The widget builds itself in
    neutral/relative layout and takes **no** placement argument: it does not
    know it is an InGame overlay, so the same `spawn` is reusable under any
    route or a test harness. Route placement is applied by glue on the returned
    entity (see `game/note_dial.rs` below). This resolves the marker-ownership
    rule: markers live with the systems that own the nodes, so the dial does
    not become the one slice where that rule lies, and the headless test "dial
    shell has hub markers" is assertable against the widget alone.
  - Rename the dial root marker `InGameDial` → `NoteDialRoot`. "InGame" is
    route vocabulary and must not live in a widget marker.
  - Rename `game/dial.rs` to `game/note_dial.rs`: call the widget `spawn`,
    then apply InGame positioning by setting the absolute `right`/`bottom`
    overlay placement on the **returned shell entity's** `Node` (set it at
    spawn-time on the existing `Node`; do not add a `Transform` to a UI node —
    AGENTS.md:51-56 / B0004). Read `MusicInfoRes` / `LatestFeatures`, update
    scene components, send `ConfigureSession`. The capture-Sa behaviour (`handle_hub_capture`,
    `sync_hub`) stays whole in glue and calls `model::hub_visual_state(...)`
    for the enum; the widget never paints the hub itself, so hover feedback
    keeps its current same-frame timing.

- Split HUD into `widgets/hud/`:
  - `model.rs`: the pure `MusicInfo -> display rows` projection (the existing
    `int_row` logic).
  - `scene.rs`: **`HudSceneRes`** — a render-facing text-row contract, kept as
    a Resource deliberately even though today it carries one row. The HUD is a
    placeholder slated to grow (more rows / readouts); the scene seam exists
    now so the growth doesn't retrofit it. (This is the Resource shape under
    "scene contracts are not one shape" below.)
  - `systems.rs`: `HudBadge`, row marker components, tree spawn, text sync
    from `HudSceneRes`.
  - Reduce `game/hud.rs` to route glue: spawn widget under the InGame root,
    refresh `HudSceneRes` from `MusicInfoRes`.
  - **Preserve the on-entry repaint explicitly.** Today `game/hud.rs` resets
    `LastMusicInfo` on entry so re-entering InGame repaints even when the
    snapshot is unchanged. Folding dedupe onto `HudSceneRes`'s own
    change-detection alone would regress this: re-entry with identical music
    info reads as "nothing changed" and the HUD paints blank. The glue must
    force a scene refresh on InGame entry regardless of change-detection
    (keep an explicit reset, or write the scene unconditionally on enter).
    A headless re-entry test must cover this (see test plan).

- Split scale picker into `widgets/scale_picker/`:
  - `model.rs`: pure row labels and selected-scale calculation.
  - `scene.rs`: row scene data. Open/closed visibility is **not** in the
    scene — see below.
  - `systems.rs`: overlay/root/rows/close-button markers and ECS tree sync.
  - Keep `game/scale_picker.rs` responsible for interactions that touch app
    state or coach commands: open sends `ListScales`, row select updates
    `SongTonality` and sends `ConfigureSession`, close hides the picker.
  - **`ShowingScalePicker` stays glue state** (route/interaction concern),
    not folded into the scene. The scene describes the rows ("given it's
    open, here is what to render"); glue owns "should it be open." This
    matches today's usage and minimises refactor risk.
  - **Fix the `sync_rows` ordering.** Today `sync_rows` is registered as an
    unordered sibling of the spawn (`lib.rs:155-160`), and the `lib.rs:150-154`
    comment wrongly claims it is chained after the spawn. It only works because
    the `ListScales` reply lands a *later* frame, by which time the rows
    entity exists. Chain spawn-then-`sync_rows` with `.chain()` so the spawn's
    `Commands` flush before the row sync reads them — this is exactly the
    same-frame sync-point rule already documented in `AGENTS.md:56-61`, which
    the picker currently violates. Delete the stale `lib.rs:150-154` comment.
    This is a deliberate correctness fix, called out as the one intentional
    behaviour change in an otherwise behaviour-preserving refactor.

- Update app wiring:
  - Update `lib.rs` resource initialization to the new `widgets::<name>::scene`
    paths.
  - Update system paths so widget systems come from `widgets::<name>::systems`,
    model projections are called by game feeders, and InGame route systems
    stay under `game::*`.
  - Preserve current scheduling semantics, especially:
    - note dial `rebuild_slots` then `apply_state` stays chained,
    - time graph scene/apply/layout capture order stays unchanged,
    - scale picker spawn then row sync is now **explicitly** chained
      (the `.chain()` fix above), replacing the prior accidental ordering.
  - **Registration placement for new widget systems.** Note-dial widget
    systems run *always-on* (`lib.rs:67-83`) while all `game::*` glue runs
    gated and `.after(coach::drain_events)` (`lib.rs:166-167`). New
    hud/scale-picker widget systems must follow the glue pattern (gated +
    `.after(drain_events)`), matching today's monolithic systems — do not
    copy the note-dial always-on registration, or risk a one-frame-lag
    regression.

- Update docs — the noun/verb split (ARCHITECTURE.md owns *what is where and
  why*; CONTRIBUTING.md owns *how you build one*) is **already done** ahead of
  implementation; see `apps/coach-game/ARCHITECTURE.md` and the trimmed
  `CONTRIBUTING.md`. Remaining doc work for the implementer:
  - Re-point the CONTRIBUTING.md time-graph example and ARCHITECTURE.md path
    references once the slice directories actually exist.
  - **Rewrite `widgets/mod.rs` and the widget header doc comments**
    (`widgets/note_dial.rs:3-5` and similar) — they state the old "widgets are
    dumb geometry; callers translate" doctrine, which #5 reverses. Point them
    at ARCHITECTURE.md and state the music-quarantine-at-`model` rule.
  - Update `lib.rs:8-16`'s module-layout doc comment, which is already stale
    and will go staler under the new paths.

## Scene contracts are not one shape

The scene layer is whatever render-facing contract fits the widget, and
today that is two shapes:

- a **Resource** (`TimeGraphSceneRes`, `HudSceneRes`) when the widget is a
  singleton fed by a projection, or
- **components on the widget entity** (`DialScale`, `DialState`) when game
  glue and widget systems meet on the entity itself.

Both are valid scene contracts. Docs should not imply uniformity.

## Public interfaces

- New public module paths for integration tests:
  - `coach_game::widgets::{note_dial, hud, scale_picker, time_graph}`,
  - `coach_game::semantic_graph`,
  - `coach_game::game::*`.

- **Flat but explicit re-exports from each widget `mod.rs`.** Each `mod.rs`
  re-exports its contract types by name — `pub use scene::{DialScale, DialState,
  ...};` and `pub use model::{the fns glue/tests call};` — so callers write
  `widgets::note_dial::DialState`, **not**
  `widgets::note_dial::scene::DialState`. Flat (no `::scene::` in caller paths)
  keeps the layer split an internal filing detail and leaves the time-graph
  test and most `lib.rs` wiring unchanged in the mechanical phase; explicit (no
  `*`) keeps private slice helpers from leaking into the public surface. Do
  not glob-export.
  - Carve-out: at system-registration sites, keep `systems` submodule-qualified
    (`widgets::note_dial::systems::rebuild_slots`) so the schedule reads as
    "these are the Bevy systems."

- Do not keep compatibility re-exports for old internal paths. Update all
  in-repo callers/tests to the new layout.

- Marker components live in the `systems.rs` of the widget that owns the
  ECS nodes (dial shell/hub markers in `widgets::note_dial::systems`, and
  likewise for hud, scale_picker, time_graph).

## Explicitly out of scope

- `ui.rs` (shared button/colour primitives), `feature_history.rs`,
  `feature_types.rs`, `coach.rs`, `state.rs`, and everything under `menu/`
  stay where they are. Do not fold them into widget slices.
- No new feature work: no label-ring vocabulary, visual editor, synth work,
  or additional UI beyond the refactor and tests.

## Test plan

- Move existing pure tests with the code:
  - semantic graph projection tests to `semantic_graph`,
  - time graph scene normalization tests to `widgets::time_graph::model`,
  - dial music projection tests to `widgets::note_dial::model`,
  - note dial current-slot geometry tests stay with `widgets::note_dial::systems`,
  - the HUD interval-row test (`int 2 2 1 2 2 2 1`, currently
    `intervals_are_the_tooth_widths` in `game/hud.rs:154`) moves to
    `widgets::hud::model` — this is a move, not a new test.

- Add missing pure tests:
  - Scale picker model renders shape labels and preserves current
    Sa/register when selecting a new shape.
  - Dial model returns no needle for no snapshot, no music, or unvoiced
    features.
  - Dial hub model distinguishes hidden, disabled, enabled, and pressed states.

- Add or update headless ECS tests:
  - Dial shell has one dial entity, hub marker, hub label marker,
    `DialScale`, and `DialState`.
  - HUD widget has badge and row markers, and refreshes text from `HudSceneRes`.
  - Scale picker opens with root/rows/close markers, repopulates rows when
    known scales change, and despawns on close.
  - Existing time graph tree test still passes against the moved scene types.

- Run verification:
  - `cargo fmt --check`
  - `cargo clippy --workspace -- -D warnings`
  - `cargo test --workspace --release`

## Assumptions

- This pass intentionally covers all current InGame UI: dial, HUD, scale
  picker, and time graph.
- The refactor should prefer identifiable structure over preserving old
  internal module paths.
- No user-visible behavior, visual styling, command order, or scheduling
  behavior should change.
