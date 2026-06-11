# Coach-game UX trace plan

A flight recorder for `apps/coach-game`: every run writes a trace of what the
app *saw* (inputs, coach output, clock) and what it *did on screen* (computed
geometry), so an AI agent can debug rendering bugs from data instead of asking
a human what the window looked like. A replay mode re-runs a recorded trace
deterministically — no mic, no DSP engine — and emits a fresh trace, turning
"is the bug fixed?" into a diff between two files.

**Why this exists.** The testing ladder
([`CONTRIBUTING.md`](../apps/coach-game/CONTRIBUTING.md)) catches bugs a level
can see, but the time-graph defects showed the gap: bugs that only manifest in
live runs (frame-batching jitter, scale-factor traps, despawn fights) burn a
human-in-the-loop cycle per attempt. The trace is the missing observability
surface: *computed* state, recorded continuously, keyed to what drove it.

## Scope boundary

Bevy-side bugs only. The trace records the feature/event stream where it
crosses the `AppCoach` port — never audio, never DSP internals. Everything
downstream of the drain (`semantic_graph`, widget models, layout, paint) is a
deterministic function of what is recorded; everything upstream is out of
scope by design.

## Frame of reference

Everything is recorded from **Bevy's point of view**, per frame. The audio
side runs on its own clock, but `coach::drain_events` already collapses that:
once per frame it drains whatever accumulated. The trace records exactly those
per-frame batches — including frames that drained nothing and frames that
drained several snapshots, because that jitter is itself a bug class. DSP-side
timestamps inside a snapshot are recorded as opaque payload; the two clocks
are never reconciled.

The one real clock problem is wall-time delta: anything reading `Time::delta`
diverges on replay if the frame rate differs. So each frame's delta is
recorded, and replay drives Bevy's clock with the recorded values
(`TimeUpdateStrategy::ManualDuration`).

## Trace format

One directory per run, one JSONL file (append-only, crash-safe — a panic
mid-run leaves every line up to the crash readable):

```
traces/2026-06-10-143212/ux.jsonl     # timestamp = app launch, UTC
```

`traces/` is relative to the working directory and gitignored. "Latest trace"
= lexicographically greatest directory name.

Each line is one record: `{"f": <frame>, "k": "<kind>", ...}`. Multiple
records per frame, written in schedule order. Kinds:

| kind | when | payload |
|---|---|---|
| `run` | once, first line | schema version, app version, window logical size, scale factor, wall-clock start, `replay_of` (replay runs only) |
| `frame` | every frame | delta seconds |
| `input` | on event | keyboard / mouse button / cursor / wheel / window resize / scale-factor change, as Bevy reports them |
| `state` | on transition | `AppState` from → to |
| `coach` | on non-empty read | what `drain_events` read this frame: polled events, `latest_features`, drained snapshots |
| `cmd` | on send | `Command` sent to the coach (context for "user clicked → what went out") |
| `geom` | on change | per-entity: widget path, physical size, global rect, clip rect, visibility, rotation, scale factor; `gone: true` on despawn |
| `mark` | F10 pressed | marker counter (reserved field: screenshot path — deferred) |

Design rules for `geom`:

- **Captured after layout** — the recording system runs in `PostUpdate` after
  `UiSystems::PostLayout` (alongside `capture_pitch_lane_size`), so it sees
  where nodes *landed*, not what was requested. This is the channel headless
  tests are blind to.
- **On-change only** — per-entity hash of the recorded fields; unchanged
  entities write nothing. Despawns are recorded explicitly (`gone`), because
  the despawn-fight bug class is precisely "something vanished that shouldn't
  have".
- **Keyed by widget path, never `Entity`** — entity ids don't survive replay,
  which would make run-to-run diffs useless. Path = `Name` ancestry joined
  with `/` plus sibling index (e.g. `time_graph/lane/trace_layer/body.3`).
  Implementation step: widget `spawn()` functions add `Name` components to
  roots and layers (also improves any future inspector tooling); raw entity id
  is included as supplementary info only.
- **Physical pixels + scale factor recorded together** — the reader can derive
  logical, and a frame-confusion bug is visible *as data* (a rect exactly 2×
  off at scale factor 2).

The volume math: inputs/coach/frame records are tiny; `geom` is bounded by
on-change hashing. A live trace repaints every frame, so its bodies dominate —
acceptable for v1; if it ever isn't, sampling policy is a one-system change.

## Recording architecture

A crate-level `trace` module in coach-game (a non-slice piece, like
`coach.rs`), wired in **`main.rs`, not `build_app`** — headless tests must not
sprout trace directories. Tests that *want* the recorder add the plugin
explicitly with a temp dir.

Two halves:

1. **Port-side: a recording decorator.** `RecordingCoach` wraps the real
   `Box<dyn AppCoach>` and logs the *outputs* of every read (`poll_events`,
   `latest_features`, `drain_features`) plus sent commands into a shared
   buffer. `drain_events` is the single reader of the handle and calls each
   method once per frame, so buffer order aligns with frames naturally. No
   change to `drain_events` itself; `spawn_coach` wraps the adapter before
   inserting the NonSend resource.

2. **Bevy-side: recording systems.** A `TraceWriter` resource (buffered file
   writer, flushed once per frame in `Last`):
   - `First`: `frame` record (`FrameCount`, `Time::delta`).
   - `Update`: `input` records from message readers; `mark` on F10; drain the
     decorator buffer into `coach`/`cmd` records (ordered
     `.after(coach::drain_events)`); `state` records from state-transition
     messages.
   - `PostUpdate` after `UiSystems::PostLayout`: `geom` on-change pass over
     all UI nodes (`ComputedNode` + `UiGlobalTransform`), despawn detection
     via the previous frame's hash map.

Serialization: the payload types crossing the port (`FeatureSnapshot`,
`CoachEvent`, `Command`, `MusicInfo`, and the domain types they carry) get
`Serialize`/`Deserialize` derives in `domain-ports` behind an optional
`serde` feature — workspace already depends on serde/serde_json. This follows
the port convention (extra data types are part of the port surface); other
consumers don't pay for it.

**Session-scoped fields are skipped, not serialized.** `InputStream::handle`
(`StreamHandle(Arc<dyn Any>)`) is session-scoped by the port's own contract
("do not persist; do not compare across adapter instances") — persisting it
would record a lie. It gets
`#[cfg_attr(feature = "serde", serde(skip, default = "null_handle"))]` with a
*private* default fn (`StreamHandle(Arc::new(()))`), so fabricating a handle
never enters the public port API. On replay, `ReplayCoach` serves devices with
these inert handles — correct, not a workaround: ReplayCoach *is* that
session's adapter instance, and the head never dereferences a handle (it reads
`persistent_id`/`name` and passes `DeviceId` back; the downcasting capture
port is explicitly future). General principle for any such field the derives
surface later: **the trace records facts that survive the session; a field the
port declares session-scoped or opaque is skipped at the serde boundary and
re-minted by the serving side.**

Doctrine check: recording computed pixels does not violate the
pixel-direction rule ([`ARCHITECTURE.md`](../apps/coach-game/ARCHITECTURE.md))
— no decision reads them; the trace is pure observability output, the same
exemption as telemetry.

## Replay mode

```
cargo run -p coach-game -- --replay [traces/<dir>]   # default: newest
```

- **No engine, no mic.** `main.rs` skips adapter construction entirely and
  inserts a `ReplayCoach` (an `AppCoach` impl shaped like the test
  `FakeCoach`: pending vecs served on read, commands logged and ignored). A
  driver system loads frame N's recorded `coach` payloads into it before
  `drain_events` runs.
- **Inputs injected at the message level** — the driver writes
  `KeyboardInput` / mouse / cursor messages early in `PreUpdate`, before
  Bevy's input-processing set, so `ButtonInput<KeyCode>` and friends update
  exactly as in the original run. Never synthetic OS events.
- **Clock driven by recorded deltas** via `TimeUpdateStrategy::ManualDuration`
  (note: the strategy read in frame N sets frame N's delta — the driver stays
  one record ahead; prime frame 0 before `app.run()`).
- **Window forced to the recorded frame** — recorded logical size +
  `scale_factor_override` on the window resolution, so geometry is comparable
  bit-for-bit even on a different display.
- **Replay records too.** The recorder runs identically, producing a new trace
  whose header carries `replay_of`. The debugging loop this enables:
  reproduce from the user's trace → fix → replay the same trace → diff the
  `geom` channels of the two runs.
- After the last recorded frame: flush and `AppExit` (agent-friendly);
  `--hold` keeps the window open for humans.

Known nondeterminism to contain, not deny: async font load (text measures
change when the Devanagari font lands — may settle on a different frame
across runs) and first-frames window setup. The round-trip test (below) runs
in the layout harness where both are controlled; live replay diffs should be
read with the first ~second treated as settling time.

## Phases

**Phase 1 — recorder. ✓ done.** serde feature in `domain-ports`; `trace` module
(writer, record types, systems); `RecordingCoach`; `Name`s on widget roots
and layers; F10 marker; gitignore `traces/`; wire into `main.rs`.
Done when: a normal run produces a `ux.jsonl` whose `geom` channel shows the
time-graph trace bodies moving inside the lane rect, and an agent can answer
"was the body inside the lane at the marker?" from the file alone.

Shipped shape: widget paths resolve as `in_game/time_graph/pitch_lane/…`
(`Name`-anchored, nameless nodes get a `#<index>` segment); `geom` records
carry physical `size_px`/`rect_px` + `scale_factor`, so logical is derived and
a 2× frame bug is visible as data; `coach`/`cmd`/`input`/`state`/`frame`
channels all populate on a live run. The session-scoped-fields rule landed as
`InputStream::handle` `#[serde(skip)]` + private `null_handle`. Replay
(Phase 2) is the only remaining half.

**Phase 2 — replay.** `--replay` flag + trace loader; `ReplayCoach` + driver
(coach payloads, input injection, manual clock, window override); replay-emits-
trace; `--hold`.
Done when: the round-trip test passes — record a synthetic run (canned
snapshots + injected inputs over N frames in the layout harness at 2×),
replay it, geometry channels compare equal modulo the run header.

**Testing.** Phase 1: headless test (recorder plugin + `FakeCoach` + temp dir
→ assert `coach`/`input` records), layout-aware test at 2× (assert a `geom`
record's physical/logical fields). Phase 2: the round-trip test, which is the
determinism contract in executable form.

## Explicitly deferred

- **Screenshots** — the `mark` record reserves a field; add capture only if a
  bug class outside the semantic trace (z-order, color, fonts) actually bites.
- **Trace diff / invariant-checker tool** — agents can grep/jq v1 traces;
  build the checker when the queries become repetitive.
- **Bevy Remote Protocol** (live querying of a running app) — complementary,
  not a substitute for post-hoc traces.
- **Replay ergonomics** — speed control, scrubbing, partial replay.
- **Retention policy** — `traces/` grows unbounded; manual cleanup for now.
- **Audio/wave recording** — out of scope permanently (see Scope boundary).
- **Recording non-geometry presentation** (text content, colors, styles) —
  geometry first; extend the `geom` record if a bug class demands it.
