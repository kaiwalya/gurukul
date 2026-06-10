# Coach-game InGame placement plan

## Summary

Replace the hand-coordinated absolute positioning of the InGame widgets
with an in-flow flex partition owned by the route, so that **non-overlap
holds by construction** instead of by arithmetic spread across three
files.

### The problem

Today the InGame screen is composed of absolutely-positioned siblings
under `InGameRoot`, and the fact that they don't overlap rests on magic
constants that secretly know about each other:

- the HUD places itself at `left: 32, top: 24`
  (`widgets/hud/systems.rs`),
- the time graph carves out `top: 96, right: 376`
  (`ROOT_*` constants in `widgets/time_graph/systems.rs`) — the `96`
  exists to clear the HUD, the `376` to clear the dial,
- the dial is a 324px box (`DIAL_BOX_PX`,
  `widgets/note_dial/systems.rs`) placed at `right: 80, bottom: 80` by
  glue (`game/note_dial.rs`).

The dial spans 80–404px from the right edge; the graph stops at 376.
The 28px gap between them exists only because three constants in three
files happen to agree. Nothing names the invariant, nothing checks it;
widen the dial or nudge its offset and the graph silently slides under
it.

There is also a doctrine violation
([`ARCHITECTURE.md`](../apps/coach-game/ARCHITECTURE.md): *widgets are
placement-agnostic; glue applies route placement*). The dial follows the
rule — its glue sets placement on the returned entity. The time graph
and HUD do not: their absolute route placement is hard-coded inside
their own `spawn`.

### The fix

Siblings *in flow* cannot overlap — flexbox computes the partition, so
there is no shared arithmetic to keep consistent. The route glue
(`game/mod.rs`) becomes the single owner of the screen partition:

```
InGameRoot                    flex column, padding: 24/32, row_gap
├─ HudSlot                    in-flow row (shrink-to-fit)
└─ ContentRow                 flex row, flex_grow: 1, column_gap
   ├─ GraphSlot               flex_grow: 1
   └─ DialSlot                fixed-width rail, justify: flex-end
```

Every magic inset disappears; the gaps become an explicit `row_gap` /
`column_gap` / `padding` in exactly one file. Absolute positioning
remains only for things that genuinely *are* overlays (the scale
picker).

> **Rule this plan commits to (goes into ARCHITECTURE.md):** widgets
> that share a screen are partitioned by an in-flow flex scaffold owned
> by the route; absolute positioning is reserved for true overlays. A
> widget never hard-codes its own route placement.

## Key changes

- **`game/mod.rs` — `spawn_root` builds the scaffold.** The root keeps
  `DespawnOnExit(AppState::InGame)` and `InGameRoot`, becomes a flex
  column with the screen padding, and spawns the slot containers shown
  above. Slots carry route-level marker components (`HudSlot`,
  `ContentGraphSlot`, `ContentDialSlot` or similar). Route vocabulary in
  these names is correct — they live in `game/`, not in a widget.
  - The `DialSlot` rail's width comes from the dial's intrinsic box.
    Expose the widget's intrinsic size (`pub const DIAL_BOX_PX` or a
    `note_dial::intrinsic_size()` accessor) rather than re-stating
    `324.0` in glue — the rail must track the widget, not copy it.
  - The current 80px bottom/right breathing room around the dial and
    the 28px graph–dial gap become the rail's padding and the row's
    `column_gap`. Match today's rendered positions approximately; exact
    pixel parity is not a goal (see Assumptions).

- **`widgets/time_graph/systems.rs` — make `spawn` placement-agnostic.**
  Delete `ROOT_LEFT/TOP/RIGHT/BOTTOM`. The root node becomes a neutral
  fill-the-parent node (`flex_grow: 1` / `percent(100)` sizing — no
  absolute insets). Everything *inside* the root (lane column,
  `LANE_GAP`, `LANE_PADDING`, the layer children) is widget-internal
  geometry and stays exactly as is.

- **`widgets/hud/systems.rs` — make `spawn` placement-agnostic.** Drop
  `position_type: Absolute` and `left/top`; the badge becomes a plain
  in-flow node. Its screen position now comes from sitting in `HudSlot`.

- **Glue spawns into slots.** `game/time_graph.rs::spawn`,
  `game/hud.rs` spawn, and `game/note_dial.rs::spawn` change their
  parent query from `Single<Entity, With<InGameRoot>>` to their slot
  marker. `game/note_dial.rs` deletes the `entry::<Node>().and_modify`
  block that applied absolute `right/bottom` — parenting into the rail
  *is* the placement now.
  - Ordering note: the slot entities are created by `spawn_root`'s
    `Commands`, so per-widget spawn systems must keep running after the
    root spawn's commands flush (they already do today via the OnEnter
    chain in `lib.rs`; preserve that chaining when touching the
    registration).

- **Scale picker: untouched.** It is a genuine overlay; absolute
  positioning over the screen root is the correct tool there.

- **Docs.** Add the route-owns-the-partition rule to
  `apps/coach-game/ARCHITECTURE.md` (a short subsection next to "Marker
  ownership"), and note the slot-marker pattern in the `game/` paragraph
  of `apps/coach-game/CLAUDE.md` if its module-layout description needs
  re-pointing. No other doc restates placement (keep it that way).

## Explicitly deferred

- **Window-resize / responsive behaviour beyond what flexbox gives for
  free.** No breakpoints, no minimum-size handling, no reflow policy.
- **Any visual redesign.** Colours, lane proportions, widget internals,
  and the dial's intrinsic size are out of scope; positions should land
  *approximately* where they are today.
- **Scale-picker work** of any kind.
- **The time-graph viewport refactor** (cutout / project-in-window —
  see ARCHITECTURE.md). Separate effort; this plan only touches the
  graph's outermost node.

## Test plan

- **Update headless ECS tests** (`tests/widget_spawn.rs` and the
  time-graph tree assertions): the time-graph root no longer has
  absolute insets, the HUD badge no longer has absolute `left/top`, and
  widget spawns are asserted under their slot parents.

- **Add the layout-aware non-overlap test** — the actual guarantee this
  plan buys. Using the existing layout-aware harness (the one behind
  `tests/time_graph_tracks_features.rs`), run the real layout schedule
  at a **2× scale factor** and assert on computed global rects:
  - HUD badge, time-graph root, and dial shell rects are **pairwise
    disjoint**,
  - the time-graph root lies inside the window and does not extend
    under the dial rail,
  - the dial shell's computed size still matches its intrinsic box
    (catches the rail squeezing the widget).

  This is the test that fails today's failure mode (widen the dial →
  rects intersect → red), which no constant-level review could catch.

- **Visual smoke check**: `cargo run -p coach-game` per
  `apps/coach-game/CLAUDE.md` (≥6s before judging), confirm HUD
  top-left, graph filling the left region, dial bottom-right.

- Run verification: `cargo fmt --check`,
  `cargo clippy --workspace -- -D warnings`,
  `cargo test --workspace --release`.

## Assumptions

- Pixel-exact parity with today's layout is **not** required; the
  partition should reproduce the current arrangement to the eye. Anyone
  needing exact values can tune the scaffold's gap/padding constants —
  now in one place.
- The dial keeps its fixed intrinsic size (it is a designed dial face,
  not a fluid region); the graph is the one elastic region.
- No scheduling or behaviour changes beyond reparenting spawns into
  slots.
