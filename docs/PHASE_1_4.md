# Phase 1.4 — Engine seam and first pixel

Goal: live mic → analyzers → something visible on a real device. Two halves:

1. **The seam** — make the engine cleanly mountable into any host. (Sections 1–4.)
2. **The cabinet** — a host app that mounts the engine, drives I/O, and renders output. (Sections 5–6.)

Status: **planning**. No code written. Revised after architect review.

---

## 1. Principle: the engine is a rack, the host is the cabinet

The engine is a pure dataflow runtime. It does not know:

- where samples come from (mic, file, synth, network)
- where outputs go (speakers, screen, log)
- whether wall-clock matters
- what platform it's running on

It only knows: *"every input port has fresh samples for this block; advance one block."*

The host is the cabinet. It mounts the engine, runs cables from its hardware (mic, speakers, screen) to the engine's faceplate, and clocks the engine by calling `process_block` once per audio callback. The same engine binary serves CI sweeps and the macOS app today, and must remain mountable from other cabinets (iOS, Android, web) without engine-side changes — only the cabinet changes.

Phase 1.4 ships only the macOS cabinet. The seam is designed for portability so future cabinets are mechanical, but no other cabinet is built in this phase.

### Design constraint: the seam must be node-shaped

A discipline, not a feature: **the engine's faceplate must be shaped such that a node could be implemented as a wrapper around a child engine**. Two flavors must both be feasible:

- **In-process sub-engine node** — a node whose `process()` forwards to a child `Engine` struct in the same binary.
- **DLL-backed node** — a node whose `process()` forwards through `engine-ffi` to a separately-loaded shared library.

Neither is shipped in the registry in Phase 1.4. The point is the test: if either flavor is awkward to build, the seam has leaked. The in-process flavor is exercised by a conformance test in PR 1.4.1 (§4); the DLL flavor is exercised on paper by walking through which `engine_*` FFI calls a `SubEngineNode` would make in PR 1.4.4.

This discipline is what keeps the seam honest. The parent always constructs the child, so sample rate and `max_block_size` are inherited at build time — no negotiation, no resampling at boundaries, no runtime block re-chunking. All realtime constraints compose: a child engine's `process_block` is realtime-safe if and only if every node inside is, which is already the project-wide rule.

The `Node` trait and the engine's faceplate are *isomorphic*, not identical. The conformance test proves the isomorphism. They remain separate types.

---

## 2. World schema change: boundary ports

The World gains two new top-level fields plus a version stamp:

```json
{
  "world_version": 1,
  "in_ports":  [ { "id": "...", "name": "...", "description": "..." } ],
  "out_ports": [ { "id": "...", "name": "...", "description": "..." } ],
  "nodes":     [ ... ],
  "edges":     [ ... ]
}
```

### Schema versioning

`world_version: 1` is added now, before the first breaking change. It's a plain integer; bump on any breaking change. The CLI rejects unknown versions. This is the moment to add it — every future breaking change otherwise reopens the same debate.

### Boundary port spec

Each port is `{ id, name?, description? }`:

- **`id`** — stable identifier. `^[a-z][a-z0-9_]*$`. Required, unique within `in_ports ∪ out_ports`. Used by edges and host APIs. Renaming is a breaking change.
- **`name`** — human label. Free string, optional. Shown in editors, tooltips, debug output. Renaming is cosmetic.
- **`description`** — free string, optional. Tooltip / API doc.

The boundary port's **type is not declared in the World** — it's derived at engine-build time from the connected node port. Single source of truth: the registry.

### Port shape propagates to the faceplate

The registry types ports as `Audio | Control | Feature` (engine/src/node.rs). That shape **must** propagate to the boundary, not collapse to "raw float slice":

- **`Audio`** — `n_frames` samples per block. Cabinet typically pipes to speakers / file / FFT.
- **`Control`** — a smooth, slowly-varying scalar. Cabinet reads the latest sample for UI display.
- **`Feature`** — an analyzer estimate with the `0.0 = unvoiced` sentinel convention. Same shape as Control for reading, but the cabinet must respect the sentinel (don't plot pitch when 0).

Onset is a `Feature` port that pulses (non-zero on event, zero otherwise). For event-shaped reads, the cabinet scans the block for the max-magnitude sample or for any non-zero — not "read the last sample." The engine's responsibility is to expose the shape; consumption pattern is the cabinet's call.

### Edges become a sum type

Edge endpoints today are `node_id.port_name`. They become either:

- `BoundaryPort(id)` — references the faceplate
- `NodePort(node_id, port_name)` — references an internal node port

In JSON, a bare identifier (no dot) is a boundary port; a dotted identifier is a node port. Examples:

```json
{ "from": "mic",            "to": "yin.in" },
{ "from": "yin.f0_hz",      "to": "pitch_hz" }
```

### Boundary input types when fanout is heterogeneous

A boundary input port can fan out to multiple node ports. **Rule: all destinations must share a `PortType`**, otherwise the World fails validation. The boundary input adopts that shared type. If you genuinely need to feed differently-typed consumers, route the boundary input through an explicit converter node — making the type change visible in the graph rather than hidden at the faceplate.

### Naming convention applied consistently

Same `{id, name?, description?}` shape applies to `NodeDef` so the schema is uniform. (Today nodes have `id` only.) `name` and `description` on nodes are additive and don't break existing Worlds.

### Validation rules

- Every `in_ports` entry has at least one outgoing edge.
- Every `out_ports` entry has exactly one incoming edge.
- Boundary port ids are disjoint from node ids.
- Boundary input ports have no incoming edges; boundary output ports have no outgoing edges.
- All destinations of a given boundary input share a `PortType`.

### Tracer's future

Tracer becomes a debug convenience, not the primary read mechanism. Sweep tests that splice a Tracer onto a port migrate to boundary outputs. Tracer is not removed in Phase 1.4 — it stays in the registry. Removal is a later-phase decision.

---

## 3. Engine API

Today: `Engine::build(world) -> Engine`, `engine.process_block(n)`, read via Tracer.

After this phase:

```rust
pub struct Engine { ... }

/// Opaque handle resolved once at build time. Index into a pre-allocated table.
/// Used on the audio thread; never causes hashing or string compare.
#[derive(Copy, Clone)]
pub struct InPortHandle(u32);

#[derive(Copy, Clone)]
pub struct OutPortHandle(u32);

#[derive(Copy, Clone, Debug)]
pub enum PortShape { Audio, Control, Feature }

pub struct BoundaryPortSpec {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub shape: PortShape,
}

impl Engine {
    /// `sample_rate` and `max_block_size` are required and immutable.
    /// Buffers are sized to `max_block_size` once, at build.
    /// `process_block(n)` accepts any `n` ≤ `max_block_size`.
    /// A sub-engine node constructs its child with the parent's SR and max_block_size,
    /// so nested engines inherit timing from the outermost cabinet.
    pub fn build(world: World, sample_rate: f32, max_block_size: usize) -> Result<Engine>;

    // --- Build-time resolution (NOT on the audio thread) ---
    pub fn resolve_in_port(&self, id: &str) -> Result<InPortHandle>;
    pub fn resolve_out_port(&self, id: &str) -> Result<OutPortHandle>;
    pub fn in_port_specs(&self) -> &[BoundaryPortSpec];
    pub fn out_port_specs(&self) -> &[BoundaryPortSpec];

    // --- Audio-thread API (realtime-safe, infallible, handle-keyed) ---
    pub fn in_port(&mut self, h: InPortHandle) -> &mut [f32];   // length == max_block_size
    pub fn out_port(&self, h: OutPortHandle) -> &[f32];
    pub fn process_block(&mut self, n_frames: usize);            // panic if n > max_block_size

    /// Drop all internal node state (filter delays, ring buffers, hysteresis).
    /// Inputs/outputs/wiring unchanged. Called after audio interruptions
    /// (route change, phone call, OS-level pause/resume) to avoid stale state.
    pub fn reset(&mut self);

    // --- Debug peek; available in all builds, not part of the host contract ---
    pub fn peek(&self, node_id: &str, port: &str) -> Result<&[f32]>;
}
```

### Realtime-safe contract

Anything on the audio thread (handle-keyed accessors, `process_block`, `reset`) is realtime-safe: no allocation, no locking, no string lookup, no thread-local storage. String-keyed APIs (`resolve_*`, `peek`) are explicitly build/debug-time only.

### Variable block size

`max_block_size` is fixed at `build` time and the engine's buffers are pre-allocated to it. Audio hosts deliver variable-size callbacks (often power-of-two, but can change on route changes — e.g., AVAudioEngine handing 4800 frames after Bluetooth swap). The cabinet chooses a generous `max_block_size` (suggest 4096) and asserts incoming buffers ≤ that. Oversized buffers are the cabinet's problem (re-chunk and call `process_block` twice).

### Sample rate

The engine has one sample rate, set at build time. The cabinet is responsible for resampling the hardware stream to the engine SR before staging into `in_port`. If the hardware rate matches, the cabinet pipes through. Sample-rate adaptation inside the engine is explicitly deferred.

### Output read concurrency

`out_port` returns a slice valid until the next `process_block`. The audio thread writes, the UI thread reads — that's a race. **The cabinet owns the boundary**: at the end of each audio callback, the cabinet copies the relevant out-port samples into a single-producer single-consumer queue or atomic snapshot. The UI thread reads from that, never directly from `out_port`. The engine doesn't double-buffer.

Why cabinet-side: the engine doesn't know what the UI cares about (latest scalar? last N? full block?). Letting the cabinet copy keeps the engine simpler and gives the cabinet the right granularity for its renderer.

### Errors on the hot path

The audio-thread accessors are infallible by construction — handles are resolved at build, so "wrong id" is impossible after build succeeds. Build-time errors return `Result` as today. FFI mirrors this: handle-keyed FFI calls return data directly, string-keyed FFI calls return `int` error codes.

---

## 4. Migration

This is a breaking schema change. Order of work:

1. **PR 1.4.1 — schema + engine support + seam conformance test.** Add `world_version`, `in_ports`, `out_ports`, sum-type edges, `{id, name?, description?}` on nodes and boundary ports. Implement boundary type derivation, handle resolution, `sample_rate` + `max_block_size` build args, `reset`, `peek`, `Node::reset()` on the node trait. Regenerate `schema/world.schema.json`. Migrate one sweep (onset) as proof-of-life. **Add a `SubEngineNode` test type** (in `engine/tests/` or a tiny `node-subengine-test` crate, not in the registry) that wraps a child Engine and forwards `process()` to it. Tier-1 test: wrap an inner analyzer World, run via the wrapper, assert identical outputs to running the inner World directly. This is the seam's conformance guard — any future change that breaks node-shape equivalence fails this test loudly.

2. **PR 1.4.2 — bulk sweep migration.** Vibrato, breath, pitch, all Tier-2 sweeps switch to boundary outputs. Tracer stays in the registry, optional.

3. **PR 1.4.3 — CLI `--peek`.** Wire the peek API to a CLI flag for ad-hoc debugging. Optional debug ergonomics; not blocking the cabinet.

PRs 1.4.1–1.4.3 ship before any host code exists. (Architect feedback: 1.4.1 + 1.4.2 + 1.4.3 was three half-states; collapsed to two atomic PRs.)

### Existing-World migration

Audit before 1.4.1:

- Test Worlds inline in sweep tests — migrated as part of 1.4.1/1.4.2.
- Any checked-in `*.json` Worlds under `worlds/` — none today; verify.
- CSV artifact shapes from sweeps — unchanged.

---

## 5. The cabinet: macOS, with portable seam

Phase 1.4 builds one cabinet — macOS. The seam constraints below are stated in terms that hold for any future cabinet too, so we don't paint ourselves into a Mac-only corner.

### Engine as a shared library

The engine compiles to a C-ABI shared library. C ABI is the lowest common denominator across Swift, Kotlin, and any other host language, and it's also what gives the cabinet enough control to call the engine from a realtime audio thread without language-runtime overhead.

Two layers:

- **`engine-ffi/`** — new crate. C-ABI wrapper around `engine`. Exposes `engine_build`, `engine_resolve_in_port`, `engine_resolve_out_port`, `engine_in_port`, `engine_out_port`, `engine_process_block`, `engine_reset`, `engine_free`. Opaque `*mut Engine` handle.
- **`engine-ffi/include/engine.h`** — `cbindgen`-generated header. One source of truth for the C API.

For Phase 1.4 the only build target is the macOS dylib (`aarch64-apple-darwin`, `x86_64-apple-darwin`). The crate itself is platform-agnostic; other targets are just additional cargo builds when a new cabinet needs them.

### FFI error handling

Two-tier scheme:

- **Build-time / string-keyed calls** (`engine_build`, `engine_resolve_*`) return `int32_t` error codes. A separate `engine_last_error_message()` returns a thread-local string for human consumption. Touches TLS — acceptable because it's not the hot path.
- **Audio-thread / handle-keyed calls** (`engine_in_port`, `engine_out_port`, `engine_process_block`) return data directly, no error path. Programmer errors (bad handle, oversized block) trip a debug assertion in Rust and are UB in release. This is intentional: handles can't be invalid after a successful build.

### Audio I/O lives in the cabinet

The engine never links CoreAudio, `cpal`, or any platform audio API. The macOS cabinet owns its audio: `AVAudioEngine` tap → buffer → call into the engine via FFI per block.

This is a load-bearing constraint, not a macOS detail: any cabinet drives the engine the same way — its native audio framework hands buffers to the cabinet, the cabinet stages them via `in_port` and calls `process_block`. Cabinets that route through a language runtime with thread-attach costs (e.g., JVM-based hosts) must call the C ABI directly from the audio callback, not through their managed bridge — otherwise the hot path pays per-callback attach overhead. This is a cabinet rule; the engine doesn't need to know.

### What the cabinet does

```
build (one-time):
  - resolve InPortHandles and OutPortHandles for every boundary port
  - allocate the SPSC queue / snapshot struct for UI hand-off

audio callback (realtime thread):
  1. mic gives us N frames as float32 (resampled to engine SR if needed)
  2. write into engine.in_port(mic_handle)[..N]
  3. engine.process_block(N)
  4. for each output handle: read engine.out_port(h), pick the right sample
     (last sample for Control/Feature; scan-for-event for Onset; full block for Audio)
  5. push compact snapshot to SPSC queue
  6. return

UI thread (not realtime):
  read from queue at 30–60 Hz, render

on interruption (route change, phone call, foregrounding):
  pause AVAudioEngine, call engine.reset(), restart AVAudioEngine
```

The realtime/UI boundary is owned by the cabinet, not the engine.

### What the cabinet does *not* do

- Define what an analyzer is. (Engine.)
- Decide which World to load. (Embed a hardcoded one for Phase 1.4; later: bundled JSON, eventually user-loadable.)
- Mutate node parameters mid-stream. (Out of scope until Phase 1.5+; the seam can grow `set_param` later.)

### What the macOS cabinet must handle

The cabinet's responsibilities (none of which touch the engine):

- Audio session setup (`AVAudioSession.setCategory(.playAndRecord)`)
- Mic permission prompt
- Interruption handling (phone call, Siri) → pause / `engine.reset()` / resume
- Route change handling (Bluetooth connect / disconnect) → pause / `engine.reset()` / resume
- Sample-rate negotiation and resampling to engine SR
- Background/foreground app lifecycle
- The SPSC / snapshot boundary between audio thread and UI thread

This list is the proof that the seam is sufficient: every concern above is the cabinet's, not the engine's. If a future cabinet on another platform finds it needs the engine to grow a new concept, that's a signal the seam leaked.

---

## 6. Phase 1.4 PR sequence

| PR | Scope |
|----|-------|
| 1.4.1 | Schema (`world_version`, boundary ports, sum-type edges, `name`+`description`) + engine API (handles, `max_block_size`, `reset`, `peek`) + one migrated sweep. |
| 1.4.2 | Bulk sweep migration. Tracer demoted. |
| 1.4.3 | CLI `--peek`. |
| 1.4.4 | `engine-ffi` crate + C header. macOS dylib build. Smoke-test from a tiny C program. **DLL-backed sub-engine conformance check**: walk through what FFI calls a hypothetical DLL-backed `SubEngineNode` would need and confirm `engine-ffi` exposes them all. On paper or as a tiny `.c` test — not a registered node. |
| 1.4.5 | macOS app skeleton. Swift Package or Xcode project under `apps/mac/`. Mic permission, `AVAudioEngine` tap, calls engine through FFI, prints pitch to stdout. |
| 1.4.6 | First pixel. SwiftUI window. Pitch number on screen that changes when you hum. |
| 1.4.7 | Visualiser: all four analyzers visible simultaneously, ECS-based entity binding (see §7). Phase 1.4 closes. |

---

## 7. Open questions

- ~~**ECS in 1.4.7.**~~ **Decided 2026-05-18: ECS slips to Phase 1.5.** Rationale: four fixed signals + four hard-wired views (pitch trace, onset ticks, breath strip, vibrato readout) — the abstraction would be invented for itself. The cost of ECS now is greater than the cost of refactoring four views into entities later, if a non-toy entity count ever appears. ROADMAP updated in the same commit (PR 1.4.7). **Phase 1.4.8's debug pane (PR 5)** is the first place a user picks a port at runtime — the exact use case ECS was originally for — but built on the simpler hardcoded `(nodeId, port) → widget` table in [`PortShape.swift`](../apps/mac/Gurukul/PortShape.swift). The deferral still stands: five ports is still well below the threshold where a component-driven view router pays for its complexity. The `DebugTapSlot` payload was deliberately shaped as data (`{port_path, type_tag, buffer}`), not a callback, so the eventual ECS refactor is mechanical: the slot's payload becomes a `PortBinding` component on a debug-tap entity, the type_tag selects the view system.
- **macOS deployment target.** Affects `AVAudioEngine` / SwiftUI APIs. Suggest macOS 13+; covers everything modern and we're not targeting old hardware.
- **Xcode project in-tree (`apps/mac/`) vs separate repo.** Recommend in-tree until it stops working — better for the part-time cadence.
- **Hardcoded World vs JSON-loaded in the cabinet.** 1.4.5 hardcodes; revisit when the editor work begins.
- **`cbindgen` vs hand-written `engine.h`.** Hand-written is fine for ~10 functions; `cbindgen` is overkill at this scale. Decide in 1.4.4.
- **Output snapshot granularity.** What does the cabinet actually push per callback — last sample per port, or some richer struct? Will become obvious once 1.4.5 is running; not worth designing upfront.

---

## 8. Explicitly deferred

Not in Phase 1.4:

- Parameter editing at runtime (`set_param` on the engine API).
- Loading user Worlds at runtime.
- Visual graph editor.
- Synth library / playback.
- Any cabinet other than macOS. (`engine-ffi` is platform-agnostic; only the macOS dylib is built.)
- Tracer removal — Tracer is demoted to "debug only" but stays in the registry.
- Registry-exposed `SubEngineNode`. The test-only sub-engine wrapper from 1.4.1 is a conformance test, not a feature. Recursive Worlds in JSON, user-authored nested graphs, and any production sub-engine node type are all out of scope.
- Notarization, signing, App Store packaging.
- Multi-window, multi-document.
- Settings UI.
- Recording / export.
- Multiple Worlds running concurrently, hot-swap.
- Push-style / event-shaped boundary ports (events are read by scanning a Feature block).
- Sample-rate conversion inside the engine.
