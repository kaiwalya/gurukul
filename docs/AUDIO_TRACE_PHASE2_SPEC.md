# Phase 2 spec — audio-trace recorder (worker tap)

> For the code-editor. Implements the **recorder** half of
> [`AUDIO_TRACE_PLAN.md`](AUDIO_TRACE_PLAN.md). Phase 1 (the headless
> replay + diff harness in `dsp-bench`) is already shipped and defines the
> on-disk artifact format. **This phase writes files Phase 1 already proved
> it can read back.**

## Goal in one sentence

When an env var names an output directory, the data-plane worker thread
records, for one live session, the exact mono 512-sample blocks it fed the
engine (a float32 WAV) plus the per-hop features it read out (a `.features.jsonl`
sidecar) plus a `.manifest.json` — in the byte-for-byte schema the Phase-1
`dsp-bench replay-audio` / `diff-features` commands consume.

## Architectural decision (SETTLED — both reviewers + me agreed, 3/3)

The artifact format (`SidecarHop`, `Manifest`) is now a **cross-layer contract**
between two crates that don't share a parent: the writer (`adapter-app-coach`)
and the reader (`dsp-bench`). Per the repo's "every fact has one source of
truth — don't duplicate, link" rule, duplicating the structs is wrong (silent
drift; a field rename would make the recorder write files Phase-1 mis-reads
with no compile error).

**Extract a tiny shared leaf crate `dsp/audio-trace-format/`** holding only the
two structs + serde derives (no logic, depends only on `serde`). Both
`dsp-bench` and `adapter-app-coach` depend *downward* on it — clean layering,
no adapter→tool inversion.

Do this DIRECTLY (do not write the placeholder structs then replace them).
Steps in §0 below.

## Background you need (read first)

The audio path: cpal RT callback → downmix to mono → SPSC ring (drops on
overflow) → the **"app-coach-data" worker thread**. The worker is the only
place that sees the exact mono block the engine eats. We tap there.

Per loop iteration the worker (`domain-adapters/app-coach/src/data_plane.rs`,
fn `run_worker`):

1. Pops exactly `BLOCK_FRAMES` (512) samples into a `block: Vec<f32>`
   (the `for slot in block.iter_mut()` loop).
2. `engine.in_port(ports.mic).copy_from_slice(&block)` then
   `engine.process_block(BLOCK_FRAMES)`.
3. Builds a `FeatureSnapshot` reading `engine.out_port(ports.X)[0]` for each
   of: pitch (`f0_hz`), confidence, onset, breath, vibrato_rate, vibrato_depth.

**The tap points are exactly (1) `block` after it is filled, and (3) the six
`out_port(...)[0]` reads.** Read sample index `[0]` — same index the worker
uses — so onset/breath (which are NOT zero-order-held across the block) match
what Phase-1 replay reads.

## §0. FIRST: extract the shared format crate `dsp/audio-trace-format/`

These types currently live in `dsp/bench/src/audio_trace.rs` (lines 24–56).
Move them verbatim into a new crate, then re-export from Phase 1 so existing
`audio_trace::SidecarHop` / `audio_trace::Manifest` paths keep compiling.

**ONE field rename while moving** (it becomes a lie once dsp-bench no longer
owns the format): `Manifest.dsp_bench_version` → `recorder_version`. Each
writer fills it with its own `env!("CARGO_PKG_VERSION")`. This is a
Phase-1-visible change: update the field in the moved struct AND every Phase-1
read/write of it in `dsp/bench/src/` (search `dsp_bench_version`).

The moved types (field order == JSONL line order; NO `t_ms`):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarHop {                 // one JSON line per hop in *.features.jsonl
    pub hop: u64,
    pub f0_hz: f32,
    pub confidence: f32,
    pub onset: f32,
    pub breath: f32,
    pub vibrato_rate: f32,
    pub vibrato_depth: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {                    // the single *.manifest.json
    pub schema: u32,                     // 1
    pub world: String,                   // basename of the world JSON path
    pub world_sha256: String,            // hex SHA-256 of the world JSON bytes
    pub sample_rate: u32,
    pub block_size: usize,               // 512
    pub channels: u32,                   // 1
    pub total_samples: usize,            // n_hops * block_size
    pub n_hops: usize,
    pub source_wav: String,              // basename of the WAV
    pub recorder_version: String,        // env!("CARGO_PKG_VERSION") of the writer
}
```

Crate setup:
- `dsp/audio-trace-format/Cargo.toml`: package `name = "audio-trace-format"`,
  edition 2021, single dep `serde = { workspace = true, features = ["derive"] }`
  (match how serde is declared in the workspace; check root `Cargo.toml`).
- `dsp/audio-trace-format/src/lib.rs`: the two structs + a short module doc
  ("the durable audio-trace artifact schema; owned here, consumed by dsp-bench
  replay/diff and written by the app-coach recorder").
- Add `dsp/audio-trace-format` to root `Cargo.toml` workspace members.
- `dsp/bench/Cargo.toml`: add `audio-trace-format = { path = "../audio-trace-format" }`.
- `dsp/bench/src/audio_trace.rs`: delete the two struct definitions; add
  `pub use audio_trace_format::{SidecarHop, Manifest};` so all existing
  internal references and the round-trip tests keep working unchanged.

The recorder (below) then `use`s `audio_trace_format::{SidecarHop, Manifest}` —
the SAME types Phase 1 reads. No duplication, no drift.

## What to build

### 1. A recorder module: `domain-adapters/app-coach/src/audio_recorder.rs`

`pub(crate)` module. Contains:

- A `Recorder` struct that owns the **writer side** of a bounded channel and a
  handle to a writer thread. Public surface (all `pub(crate)`):

  ```rust
  /// Activation: returns Some only if the env var GURUKUL_AUDIO_TRACE_DIR
  /// is set and non-empty. `dir` (from the env var) is where files go.
  /// `stamp_ms` is the filename stamp — the worker passes `clock.now_ms()`
  /// (this crate must NOT read a wall clock itself). `world_json` is the
  /// exact world bytes (for the SHA-256); `world_name` is the logical
  /// basename for the manifest (e.g. "coach.json").
  pub(crate) fn from_env(
      sample_rate: u32,
      stamp_ms: u64,
      world_json: &str,
      world_name: &str,
  ) -> Option<Recorder>;

  /// Push one consumed block. Realtime-adjacent (called in the worker hot
  /// loop) — must not block: `try_send` on a bounded channel; on a full
  /// channel set the invalid flag and drop the data.
  pub(crate) fn record_block(&mut self, block: &[f32]);

  /// Push one hop's features. Same non-blocking contract.
  pub(crate) fn record_hop(&mut self, hop: SidecarHop);

  /// Called once at worker shutdown. DROPS the sender (the writer loop
  /// treats channel-closed as its finish signal), JOINS the writer thread,
  /// then — only if neither backpressure NOR any writer I/O error
  /// invalidated the run — writes the manifest. There is NO blocking
  /// "finish" message (a full bounded channel could drop or stall it).
  pub(crate) fn finish(self, telemetry: &dyn Telemetry);
  ```

### 2. The Heisen-recording guard (critical)

The recorder must **never stall the worker**, because a stall would itself
cause the ring drops the recording is meant to faithfully capture. So:

- `record_block` / `record_hop` send on a **bounded** `sync_channel`
  (capacity ~256 messages — enough to ride out disk hiccups, small enough to
  bound memory). Use `try_send`.
- On `TrySendError::Full`, set a **shared** `Arc<AtomicBool> invalid` flag and
  **drop the message**. Do not block, do not grow.
- **The writer thread ALSO sets the same `invalid` flag on any I/O failure**
  (WAV `write_sample`, sidecar write, or `.finalize()` erroring). Backpressure
  is not the only way an artifact goes bad — a disk-full or finalize error
  produces a short/corrupt file that must not get a manifest either. The flag
  is shared (`Arc`) precisely so both the worker side (channel-full) and the
  writer side (I/O error) can raise it.
- On `finish`, if `invalid` is set: log a `tel_warn!` that the recording was
  invalidated (backpressure or writer I/O error), and **do not write the
  manifest** (an incomplete artifact must not masquerade as complete). The
  partial WAV + sidecar may remain on disk but without a manifest Phase-1
  replay will refuse them — the desired fail-closed behavior.

### 3. The writer thread

Spawned in `from_env`. Owns the receiver end and the shared `invalid` flag.
Loop on `recv()`:

- `Ok(WriterMsg::Block(Vec<f32>))` → append samples to the WAV writer; on
  error, set `invalid` and keep draining (don't panic).
- `Ok(WriterMsg::Hop(SidecarHop))` → write one `serde_json` line + `\n` to the
  sidecar file; on error, set `invalid`.
- `Err(_)` (channel closed — the worker dropped the sender in `finish`) →
  flush + `.finalize()` the WAV (set `invalid` if finalize errors), then
  break. **This is the only finish trigger — there is NO `WriterMsg::Finish`
  variant.** So `WriterMsg` is just `{ Block(Vec<f32>), Hop(SidecarHop) }`.

Use `hound::WavWriter` with `WavSpec { channels: 1, sample_rate,
bits_per_sample: 32, sample_format: hound::SampleFormat::Float }`. Write each
sample with `write_sample(s: f32)`.

The **manifest** is written by `finish()` on the worker side AFTER the writer
thread is joined, because `n_hops` / `total_samples` are only known then.
Count hops as you send them (a counter on the `Recorder`), so `finish` knows
`n_hops` without re-reading the file. `total_samples = n_hops * block_size`.
Skip the manifest entirely if `invalid` is set after the join.

### 4. File naming

Mirror the UX trace's stamp convention loosely. The stamp value must come from
the worker's `clock.now_ms()` (passed in as `stamp_ms` — this crate does NOT
read a wall clock itself). Form a numeric stamp prefix from `stamp_ms`. Files:

- `<dir>/<stamp>-engine-input.wav`
- `<dir>/<stamp>-engine-input.features.jsonl`
- `<dir>/<stamp>-engine-input.manifest.json`

(`source_wav` in the manifest = the WAV basename; `world` = the passed
`world_name`.)

> Naming note: Phase-1's `replay-audio` derives sidecar/manifest stems from the
> WAV stem by stripping the `.wav` extension and appending `.features.jsonl` /
> `.manifest.json`. Keep the recorder's names consistent with that derivation
> so a recorded WAV can be re-replayed by Phase 1 and land its outputs beside
> the originals (different stem is fine; same *derivation rule*).

### 5. Wire into the worker

In `data_plane.rs`:

- `from_env` is called once near the top of `run_worker`, AFTER the engine and
  ports are built (so we have `sample_rate`). Pass `clock.now_ms()` as
  `stamp_ms`, the world JSON string, and the logical world name. Store
  `Option<Recorder>` in a local `mut recorder`.
- After `block` is filled and before `process_block`: `if let Some(r) =
  recorder.as_mut() { r.record_block(&block); }`.
- After the `FeatureSnapshot` is built: build a `SidecarHop` from the SAME six
  `out_port(...)[0]` values (capture them in locals, build BOTH the
  `FeatureSnapshot` and the `SidecarHop` from those locals — do NOT re-read the
  ports) keyed by the worker's `hop_index`, then `r.record_hop(hop)`.
- **One exit, one finish.** The pop loop currently has an early `return` when
  the producer is dropped. Restructure it to `break` out of the `while` loop
  instead, so there is exactly ONE place after the loop where the recorder is
  finished: `if let Some(r) = recorder.take() { r.finish(&*telemetry); }`,
  placed before the final `tel_info!("worker down")`. Do NOT scatter `finish`
  across multiple `return` sites — a future third exit path would silently skip
  it. (Confirm the `break`-instead-of-`return` refactor doesn't change the
  existing feature-producer-on-unwind handling; the producer is preserved by
  the wrapping `preserve_feature_producer_on_unwind`, so falling through to the
  end of the fn is equivalent to the old `return` for that purpose.)

### 6. Exposing the world JSON to the recorder

`pitch_world.rs` ALREADY has the embedded world bytes — a private
`const COACH_WORLD_JSON: &str = include_str!("../../../dsp/worlds/coach.json");`
(line 11). Just make it `pub(crate)` so the worker can pass it to the recorder.
The logical `world_name` is `"coach.json"`. The recorder computes
`world_sha256` with `sha2::Sha256` over those bytes (hex). **Add `sha2 = {
workspace = true }` to `adapter-app-coach`'s `Cargo.toml`** (the `sha2 = "0.10"`
workspace dep already exists). Likewise add `serde = { workspace = true }` and
`hound = "3.5"` if not already present (`serde_json` is already a dep).

## Tests (in `data_plane.rs` or the recorder module)

1. **Recorder off by default.** With the env var unset, `from_env` returns
   `None`. The existing `sine_440_round_trips_to_publisher` test still passes
   (no behavior change when not recording).
2. **Records a known signal.** Set the env var to a `tempfile::tempdir()` path,
   run a short synthetic session (reuse the sine-440 test harness — feed N*512
   samples), `finish`, then assert: the WAV exists and has `n_hops * 512`
   samples; the `.features.jsonl` has `n_hops` lines each parsing as JSON with
   the seven fields; the manifest exists with `schema==1`, `block_size==512`,
   `channels==1`, `n_hops` matching, and a 64-hex-char `world_sha256`.
3. **Re-deserializes with the shared crate.** Read the written
   `.features.jsonl` and `.manifest.json` back and deserialize them with
   `audio_trace_format::{SidecarHop, Manifest}` (the SAME types Phase 1 uses) —
   asserting the round-trip. This gives most of the round-trip confidence
   **without** a `dsp-bench` dev-dependency on the adapter (which would
   reintroduce the layering inversion we just removed). The full end-to-end
   `dsp-bench diff-features` against a recorded file is the manual **step-5**
   check, not a unit test.

`adapter-app-coach` already depends on `audio-trace-format` (a normal dep, for
the recorder). Use the `tempfile` crate as a **dev-dependency** so tests never
write into the repo.

## Constraints / non-goals

- **Do not** change the `AudioConfig` / `StartSession` port types. Activation is
  the env var only — the coach-game app (Phase 3) will set it before launching.
- **Do not** record the partial final block (the worker already only processes
  whole 512-blocks; just don't invent a flush of leftovers).
- **Do not** touch the RT callback / `push_samples`. The tap is worker-side.
- **Do not** add the YIN fix. This is infrastructure only.
- Keep `cargo fmt`, `cargo clippy --workspace -- -D warnings`, and
  `cargo test --workspace --release` clean. Prefer `is_multiple_of` over
  `% n == 0` (clippy lints the latter).
