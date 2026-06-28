# Plan: Plugin Delay Compensation (PDC) for the vibrato band

*Reviewed by the software-architect agent — verdict: sound, 5 corrections folded in.*

## Problem
The vibrato analyzer node measures wiggle over a 1.5s window, so its output describes
audio ~0.75s old (group delay = window/2). But the host stamps pitch and vibrato with the
SAME `t_ms = clock.now_ms()` when it reads the engine outputs. They share a timestamp but
not an age, so the vibrato band renders behind the pitch trace it should envelope.

## Goal
Land the deferred PDC mechanism (`dsp/ARCHITECTURE.md` line 27, lines 52-57) so vibrato
features are timestamped with their true age, and the band lines up under the trace.

The architecture mandate: "each node declares its inherent latency; engine aligns
downstream consumers." And: "This is VST3/CLAP/AU with the names changed. Do not invent a
new protocol — scope the existing pattern to this domain."

Narrowing (architect-confirmed, faithful to the mandate): there is no in-graph node
downstream of vibrato — the **host is the only consumer**. So the engine *reports* latency
and the *host* aligns. That is scope discipline, not protocol invention.

## Key facts established from the code
- `Node` trait (`dsp/engine/src/node.rs`): `prepare` / `process` / `finish` / `reset`. No
  latency method today. ARCHITECTURE.md names the deferred method `declare_latency()`.
- Engine (`dsp/engine/src/graph.rs`) already maps each boundary out-port back to its
  producing node: `out_port_sources[handle] = (src_node_idx, src_port_idx)`. So given an
  out-port handle, the engine can reach the node behind it.
- Vibrato node (`dsp/node-vibrato/src/lib.rs`): `window_samples = 72000` (1.5s @ 48k),
  `analysis_hop = 4800` (0.1s), `decimation = 256`.
- Latency is a FIXED property (a constant), not a per-frame signal — so it is a
  side-channel query, NOT a boundary out-port/pin. (Pins stream changing f32s every block;
  latency is one integer read once at boot.) This matches VST/CLAP: audio flows on the
  jack, the spec sheet states the latency.
- Host publish seam (`domain-adapters/app-coach/src/data_plane.rs` ~378-403): reads each
  out-port `[0]`, builds one `FeatureSnapshot { ..., t_ms: clock.now_ms() }`.

## The latency value — window center PLUS hold lag  (correction #1)
Two terms, both real:
- **Window center:** the estimate describes the MIDDLE of the 1.5s window →
  `window_samples / 2` = 36000 frames = 0.75s.
- **Zero-order-hold lag:** the node only re-runs analysis every `analysis_hop` (0.1s) and
  holds the value between runs, so a held estimate is on average `analysis_hop / 2`
  (2400 frames = 0.05s) older than its window center.
- **Decimation adds nothing** — it downsamples the *same* window; the time span (and thus
  the center) is unchanged.

So: **`declare_latency() = window_samples / 2 + analysis_hop / 2`** = 38400 frames = 0.80s.
The pitch trace has ~zero latency, so every ms of vibrato lag shows as misalignment —
the 50ms hold term is worth including.

## Design (4 steps)

### 1. `Node` trait — add the latency declaration
Add a default method to the trait in `dsp/engine/src/node.rs`:
```rust
/// Inherent processing latency in FRAMES at the prepared sample rate.
/// Valid after `prepare()` (a sample-rate-dependent node computes it there).
/// Default 0 — a node with no look-behind / look-ahead delay.
fn declare_latency(&self) -> usize { 0 }
```
- Unit is **frames** (sample-rate-agnostic at the trait, matches VST3 `getLatencySamples`
  / CLAP latency-in-samples).  (correction #3/#5)
- Default impl → every existing node unaffected.
- Contract pin: latency is valid AFTER `prepare()`. The host queries post-`Engine::build`
  (build calls prepare), so this holds.

### 2. Vibrato node — override it
In `dsp/node-vibrato/src/lib.rs`:
```rust
fn declare_latency(&self) -> usize {
    self.window_samples / 2 + self.analysis_hop / 2
}
```

### 3. Engine — expose latency per out-port
Add to `Engine` in `dsp/engine/src/graph.rs`:
```rust
/// Inherent latency (frames) of the node feeding boundary out-port `h`.
/// Boot-time lookup (walks out_port_sources → node). NOT hot-path.
pub fn out_port_latency(&self, h: OutPortHandle) -> usize {
    let idx = h.0 as usize;
    debug_assert!(idx < self.out_port_sources.len(), "OutPortHandle out of range");
    let (node_idx, _) = self.out_port_sources[idx];
    self.nodes[node_idx].1.declare_latency()
}
```
`debug_assert!` matches the existing `in_port` / `out_port` accessors.  (correction #4)

### 4. Host — back-date the vibrato stamp  (correction #2 + Open Question = A)
In `data_plane.rs`:
- **At boot** (off hot path), read latency once and convert frames→ms in floating point
  with rounding (NOT integer truncation):
  ```rust
  let lat_frames = engine.out_port_latency(ports.vibrato_rate);
  // vibrato_rate and vibrato_depth share a node → assert equal latency.
  debug_assert_eq!(lat_frames, engine.out_port_latency(ports.vibrato_depth));
  let vibrato_latency_ms =
      (lat_frames as f64 * 1000.0 / sample_rate as f64).round() as u64;
  ```
- **In the publish loop**, stamp vibrato with its own aged time:
  ```rust
  let t_ms = clock.now_ms();
  let snapshot = FeatureSnapshot {
      // ... pitch/confidence/onset/breath share t_ms ...
      vibrato_t_ms: t_ms.saturating_sub(vibrato_latency_ms),
      t_ms,
  };
  ```
  Only the hot-path delta is one subtraction. No alloc, no lock.

This is **Open-Question option A** (the data carries its own age), and the architect's
refinement: frame it as a PRECEDENT, not a vibrato hack — *any feature whose source node
declares latency carries its own aged stamp; features with no declared latency share
`t_ms`*. Keeps engine-latency knowledge OUT of the renderer (rejecting option B) and keeps
time resolved at the host edge, the one place that owns the clock.

### 5. Head — the time-graph projection reads `vibrato_t_ms`
The model layer already normalizes points against the rolling window by their timestamp.
The vibrato band's points get normalized by `vibrato_t_ms` instead of `t_ms`, sliding the
band ~0.80s forward to sit under the trace. (Exact wiring located during implementation.)

## Realtime discipline
- `declare_latency()` `&self`, returns a constant — no alloc/lock; called only at boot.
- `out_port_latency()` is a boot-time lookup, never in `process_block`.
- Hot-path change is one subtraction. Clean.

## Out of scope (deferred, per ROADMAP scope discipline)
- Multi-hop PDC (a node downstream of vibrato that must realign): no such node exists; the
  host is the only consumer. Build the host-consumer path, not a general graph realigner.
- The separate scalloping fix (flat band rails) — revisit AFTER alignment lands.
- `serialize_state` / event ports — other deferred trait methods, not this change.

## Test plan
- Unit (engine): a node with `declare_latency() = N` wired to an out-port →
  `out_port_latency(handle) == N`. Default-0 node → 0.
- Unit (vibrato): `declare_latency() == window_samples / 2 + analysis_hop / 2`.
- Unit (host conversion): frames→ms rounds correctly (e.g. a non-divisor latency).
- On-device: replay `highlighter_test.wav`, screenshot — band should now sit UNDER the
  trace (user verifies on phone).

## Files that change
- `dsp/engine/src/node.rs` — trait method
- `dsp/engine/src/graph.rs` — `out_port_latency`
- `dsp/node-vibrato/src/lib.rs` — override
- `domain-adapters/app-coach/src/data_plane.rs` — `FeatureSnapshot` field + back-date
- `apps/coach-game/src/...` time-graph projection — normalize vibrato by `vibrato_t_ms`
