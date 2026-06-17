# Contributing to `coach-game`

*How to build a widget.* For **what** the layers are and **why** the crate is
shaped this way (the slice doctrine, music quarantine, scene shapes, marker
ownership), see [`ARCHITECTURE.md`](ARCHITECTURE.md). For how the head
compiles and packages per platform (iOS/Android), see [`BUILD.md`](BUILD.md).

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

## Debugging live runs from the trace

The ladder above ends at layout-aware tests, and every level is blind to
something. Bugs that only exist *live* — viewport jitter, frame-batching
glitches, despawn flicker, anything a human reports as "it looks wrong" —
have a fourth surface: every `cargo run` writes a UX trace (file location
and mechanics in [`AGENTS.md`](AGENTS.md); record schema in
`src/trace/record.rs`; design rationale in `src/trace/mod.rs`).
Saw the bug happen? Press **F10 in the moment** — it writes a `mark` record
that turns "I saw it flicker once" into a frame number.

Read a trace the way the recorder wrote it: effects in one channel, causes
in the others, aligned by the frame field `f`.

1. **Find the symptom in `geom`.** This channel is *computed* geometry —
   where nodes actually landed after layout, in physical px — so a visual
   defect is present as data. Ask what the doctrine says *should* be true
   and query for the violation: bodies inside their lane's rect, gridlines
   repainting rarely, entity paths not flapping `gone`.
2. **Find the cause in `coach` / `input`.** These channels preserve
   per-frame batching jitter on purpose. Line them up with the symptom
   frames; an exact frame-correlation is the recorder handing you the
   causal arrow.
3. **The decision between them is code.** The trace shows what went in and
   what came out; the wrong decision lives in whichever layer maps one to
   the other — and the correlation names it. Go read that layer; don't
   guess from pixels.

Worked example — "the pitch trace looks jumpy, like the zoom is bouncing":

```sh
# Symptom: gridlines should repaint only on viewport change, yet…
gzcat traces/<stamp>-ux.jsonl.gz | \
  jq -r 'select(.k=="geom" and (.path|contains("gridline_layer/")))
         | "\(.f) \(.rect_px)"'
# …they repainted on 34 of ~60 InGame frames, line spacing flapping
# 151→111→77→66 px: pan AND zoom bouncing. Cause: which frames got data?
gzcat traces/<stamp>-ux.jsonl.gz | \
  jq -r 'select(.k=="coach") | "\(.f) \(.drained)"'
```

(Use `gunzip -c` in place of `gzcat` if preferred. This works because a
graceful exit — window close / Cmd-Q — finalizes the gzip stream (and even a
panic finalizes it on unwind). Only a hard kill (`kill -9`, abort) leaves a
trailerless trace; on macOS `gzcat`
and `gunzip` then emit **nothing** at all. Recover such a trace with a
tolerant decoder — `python3 -c 'import zlib,sys;sys.stdout.write(zlib.decompressobj(31).decompress(open(sys.argv[1],"rb").read()).decode("utf-8","ignore"))' traces/<stamp>-ux.jsonl.gz | jq …`
— or simply `--replay` it, since the loader recovers every flushed line.)

Every repaint frame coincided exactly with a voiced pitch sample arriving —
sparse, low-confidence (0.2–0.4) samples, one a 3-octave outlier. Diagnosis,
without a screenshot or a human in the loop: the viewport policy follows the
raw data extent with no damping, and low-confidence points are admitted to
the extent. Both fixes are domain-side policy (the projector), per the
domain-decision rule.

**Close the loop with replay.** A trace is not just evidence — it is an
executable reproduction:

```sh
cargo run -p coach-game --release -- --replay   # newest trace; or --replay traces/<file>
```

No mic, no engine: the app re-runs against the recorded inputs, coach
reads, and clock deltas, and emits a fresh trace whose header carries
`replay_of`. Replay is deterministic (live input is suppressed; the
recorded stream is the only stream), so a fix is verified by *diffing*,
not eyeballing: replay the bug trace on the fixed code and diff the
`geom` channels —

```sh
diff <(gzcat traces/<bug-run>-ux.jsonl.gz | jq -c 'select(.k=="geom")') \
     <(gzcat traces/<replay>-ux.jsonl.gz  | jq -c 'select(.k=="geom")')
```

On unfixed code that diff is empty — bit-for-bit; that is the contract,
and `tests/trace_replay_roundtrip.rs` is it in executable form (one
caveat: treat the first ~second as settling time — async font load can
land on a different frame).
On fixed code the divergence should be exactly the change you intended,
nothing else.
`--hold` keeps the window open after the last frame when you do want to
look. Mechanics in [`AGENTS.md`](AGENTS.md); flags in `src/main.rs`.

What the trace is blind to: z-order, color, text rendering — anything
outside the recorded fields. If a trace says "fine" while eyes say
"broken", suspect those — a screenshot hook is deliberately deferred until
that bug class actually bites (the `mark` record reserves a field for it;
see the deferred list in `src/trace/mod.rs`).

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
