# Phase 1 spec: headless audio replay + feature-sidecar diff

> Implementation spec for a code-editor agent. Self-contained: do not infer
> beyond what is written here. When a choice is not specified, prefer the
> smallest change and match surrounding code. Parent design:
> `docs/AUDIO_TRACE_PLAN.md`.

## Goal

Add a CLI surface to the existing `dsp-bench` binary that:

1. **`replay-audio`** — runs a recorded mono WAV through the coach pitch engine
   and writes a **feature sidecar** (`.features.jsonl`) + a **manifest**
   (`.manifest.json`).
2. **`diff-features`** — compares two feature sidecars and prints a report
   (octave-jump counts, jitter, voiced coverage, per-hop divergence).

This is the *consumer* half of the audio-trace system. Building it first means
the recorder (Phase 2) writes into a format already proven replayable. **There
is no recorder in this phase** and **no Bevy** — this is pure DSP + file I/O in
the `dsp/bench` crate.

## Why this shape (read before coding)

The machinery already exists and **must be reused, not reinvented**:

- `dsp/bench/src/lib.rs` already has `Bench::mount(path).bind("mic",
  Source::wav(p)).capture_out([...]).run(Run::...)` — the entire
  WAV→engine→capture loop, block-by-block, zero-padding short files.
- `Captured` already implements `per_hop(path)`, `octave_jumps(path)`
  (600-cent threshold), `median_jitter_cents(path)`, `coverage_voiced(path)`.
- `read_wav_mono` already reads float + int WAV at an asserted sample rate.
- `hound` is already a dependency of the `dsp-bench` crate.
- The CLI already uses `clap` with a `Command` subcommand enum
  (`dsp/bench/src/main.rs`).

So the new code is: **two CLI subcommands**, a **sidecar/manifest
serialization module**, and **tests**. Do not write a new WAV reader, engine
loop, or octave-jump metric — call the existing ones.

## The coach world and its ports

`dsp/worlds/coach.json` is the world to mount. Its boundary ports (confirmed in
`domain-adapters/app-coach/src/data_plane.rs:392-423`) are:

- in: `mic`
- out: `pitch`, `confidence`, `onset`, `breath`, `vibrato_rate`, `vibrato_depth`

A `FeatureSnapshot` (defined in `domain-ports/src/app_coach.rs`) is one hop's
reading of all six out-ports plus `hop_index` and `t_ms`. The sidecar is a
stream of these, one JSON object per line. **Do not import or reuse
`FeatureSnapshot`** — the sidecar uses a *new* struct `SidecarHop` (defined
below) that omits `t_ms`. They are deliberately different types.

### CRITICAL: read sample-0 of each block, NOT the last sample

The live worker publishes each feature as `out_port(<port>)[0]` — the **first**
sample of the block (`data_plane.rs:359-364`). The headless replay **must read
the same sample** or its sidecar will not match a future recording.

Why this matters and is not pedantic: four of the six ports are
**zero-order-hold** (pitch, confidence, vibrato_rate, vibrato_depth) — they
`fill()` the whole block with one constant, so sample 0 == sample 511. But
**onset** (a sparse impulse — `1.0` only at the firing sample,
`node-onset`) and **breath** (a per-sample latch that can flip mid-block,
`node-breath`) are **not** held constant. For those two, sample 0 ≠ sample 511
in exactly the blocks that carry an event.

Therefore: **do NOT use `Captured::per_hop(...)`** for the sidecar. `per_hop`
takes `.last()` (sample 511) and would silently diverge from the worker on
onset/breath. Read **`Captured::out(port)[i * block_size]`** (sample 0 of hop
`i`) instead. (The worker's own `[0]` read is itself lossy for sparse onset —
an impulse mid-block is invisible at sample 0 — but that is a pre-existing
worker quirk; our contract is "match the worker," not "capture the true
event." Out of scope to fix here.)

## File: feature sidecar + manifest schema (NEW)

Create `dsp/bench/src/audio_trace.rs`. This module owns the durable artifact
format — it is the contract Phase 2's recorder will also write to, so define it
deliberately here.

**Module placement (do not get this wrong):** `audio_trace` is a **`pub mod` of
the library crate** — add `pub mod audio_trace;` to `dsp/bench/src/lib.rs`, NOT
to `main.rs`. It must be reachable by *both* `Captured::octave_jumps` (in
`lib.rs`) and the `cmd_*` functions (in `main.rs`). The binary uses it as
`dsp_bench::audio_trace::...`; `lib.rs` uses it as `crate::audio_trace::...`.

**The testable core lives here, not in `main.rs`.** Define:

```rust
/// Run `samples` (already truncated to a whole number of blocks) through the
/// world's pitch engine and produce the sidecar hops + manifest. No file I/O —
/// `cmd_replay_audio` does the WAV-read and file-write around this.
pub fn replay_samples(
    samples: &[f32],          // length MUST be a multiple of block_size
    sample_rate: u32,
    block_size: usize,
    world_path: &std::path::Path,
    world_sha256: &str,       // computed by the caller from the file bytes
    source_wav: &str,         // basename, for the manifest
) -> anyhow::Result<(Vec<SidecarHop>, Manifest)>
```

It calls `dsp_bench::Bench::mount(world_path)` directly (same crate). This is
the function test #4 exercises without touching the filesystem.

### Sidecar: `<stem>.features.jsonl`

One JSON object per line (JSONL). Each line is one hop:

```json
{"hop":0,"f0_hz":0.0,"confidence":0.0,"onset":0.0,"breath":0.0,"vibrato_rate":0.0,"vibrato_depth":0.0}
```

- Field names exactly as above. `hop` is a `u64` starting at 0.
- **Do NOT include `t_ms`.** It is wall-clock, differs every run, and is
  excluded from every diff. The headless replay has no meaningful wall-clock.
- Use `serde` with a struct `SidecarHop` deriving `Serialize, Deserialize`.
  Serialize each hop with `serde_json::to_string` + `writeln!`.

### Manifest: `<stem>.manifest.json`

A single JSON object pinning everything needed to reproduce the engine:

```json
{
  "schema": 1,
  "world": "coach.json",
  "world_sha256": "<hex>",
  "sample_rate": 48000,
  "block_size": 512,
  "channels": 1,
  "total_samples": 480768,
  "n_hops": 939,
  "source_wav": "session-2026-06-12.wav",
  "dsp_bench_version": "0.1.0"
}
```

- `schema`: `u32` literal `1`. Bump only on a breaking format change.
- `world`: the world filename (basename of the mounted path).
- `world_sha256`: hex SHA-256 of the world JSON file bytes. **`sha2` is NOT
  currently a dependency of `dsp/bench/Cargo.toml` (confirmed) — add
  `sha2 = "0.10"`.** Hash the **raw bytes on disk**: `std::fs::read(world_path)`
  → `Sha256::digest` → hex. (`Bench::mount` reads the file a second time; that
  double-read is fine — do not try to share the read.) This pins the node graph
  so a replay against a changed `coach.json` is detectable.
- `sample_rate`, `block_size`, `channels`: from the run config (channels is
  always 1 — the sidecar/WAV is mono engine-input).
- `total_samples`: the number of mono samples actually fed to the engine =
  `n_hops * block_size` (see partial-block policy below).
- `n_hops`: number of hops in the sidecar.
- `source_wav`: basename of the input WAV.
- `dsp_bench_version`: `env!("CARGO_PKG_VERSION")`.

Use a struct `Manifest` deriving `Serialize, Deserialize`. Write with
`serde_json::to_string_pretty`.

### Partial-block policy (CRITICAL — this is the format contract)

The live worker (Phase 2) only ever processes **whole 512-sample blocks**; a
partial final block is never fed to the engine. The headless replay **must
match this exactly** so a recording and its replay produce identical hop
counts.

- Number of hops = `wav_samples / block_size` using **integer (floor)
  division**. The trailing `wav_samples % block_size` samples are **dropped**,
  not zero-padded.
- `total_samples` in the manifest = `n_hops * block_size` (the samples actually
  consumed), NOT the WAV's raw length.
- **Do not use `Bench`'s `Run::Secs`/`materialise` zero-padding path for this.**
  `Bench::materialise` zero-pads to fill the last block; that would invent a
  hop the live worker never produces. Instead, read the WAV yourself via the
  reused reader, compute `n_hops` by floor division, and run exactly `n_hops`
  blocks. (You may use `Bench` with `Run::blocks(n_hops)` + `Source::samples`
  of the floored sample buffer — `Source::Samples` is also zero-padded by
  `materialise`, but if you pass exactly `n_hops*block_size` samples there is
  nothing to pad. Confirm: `materialise(total, _)` resizes to `total`; pass
  `total == n_hops*block_size`.)

This is the single most important correctness point in the phase.

## File: `replay-audio` subcommand

Add to the `Command` enum in `dsp/bench/src/main.rs`:

```rust
/// Replay a recorded mono WAV through a world's pitch engine and write a
/// feature sidecar (<stem>.features.jsonl) + manifest (<stem>.manifest.json).
ReplayAudio {
    /// Path to the input mono WAV (engine-input samples).
    wav: PathBuf,
    /// World JSON to mount (default: dsp/worlds/coach.json).
    #[arg(long, default_value = "dsp/worlds/coach.json")]
    world: PathBuf,
    /// Output stem. Sidecar/manifest are <stem>.features.jsonl and
    /// <stem>.manifest.json. Default: the WAV path with extension stripped.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Sample rate the WAV must be at (asserted, not resampled).
    #[arg(long, default_value_t = 48000)]
    sample_rate: u32,
    /// Block size (must equal PitchYin hop; default 512).
    #[arg(long, default_value_t = 512)]
    block_size: usize,
},
```

Implementation (`cmd_replay_audio`, in `main.rs`). It is a thin shell around
`audio_trace::replay_samples` — WAV-read and file-write only:

1. Read the WAV mono samples at `sample_rate` (reuse the bench's reader — see
   "Reader reuse" below).
2. Compute `n_hops = samples.len() / block_size` (floor). If `n_hops == 0`,
   bail with a clear error ("WAV shorter than one block"). Truncate samples to
   `n_hops * block_size`.
3. Compute `world_sha256` = hex SHA-256 of `std::fs::read(&world)`.
4. Call `audio_trace::replay_samples(&truncated, sample_rate, block_size,
   &world, &world_sha256, <wav basename>)` → `(Vec<SidecarHop>, Manifest)`.
   (Steps 4-5 of the OLD list — the `Bench` run and the sample-0 reads — live
   *inside* `replay_samples`, per the "CRITICAL" section above.)
5. **Derive output paths** from `out` (default `wav.with_extension("")`):
   - sidecar path = `out` with `.features.jsonl` appended, manifest =
     `.manifest.json` appended. **Beware `Path::with_extension` replaces, not
     appends** (`session.wav.with_extension("features.jsonl")` →
     `session.features.jsonl`, which is fine; but to be safe build the names by
     string concatenation on the stem: `format!("{stem}.features.jsonl")`).
     Spell it: `let stem = out.to_string_lossy(); let sidecar =
     PathBuf::from(format!("{stem}.features.jsonl")); let manifest =
     PathBuf::from(format!("{stem}.manifest.json"));`
6. Write the sidecar (one `serde_json::to_string(&hop)` + `writeln!` per hop)
   and the manifest (`serde_json::to_string_pretty(&manifest)`).
7. Print a one-line summary to stderr: hops written, octave jumps
   (`audio_trace::count_octave_jumps` over the `f0_hz` series), output paths.

### Reader reuse

`read_wav_mono` in `dsp/bench/src/lib.rs` is currently **private** (`fn`, module
scope). Make it **`pub`** (the CLI integration test in `dsp/bench/tests/` is a
separate crate and may want it; `pub` is the safe choice). Do not duplicate
WAV-reading logic.

## File: `diff-features` subcommand

Add to the `Command` enum:

```rust
/// Diff two feature sidecars (baseline vs candidate) and report changes.
DiffFeatures {
    /// Baseline sidecar (.features.jsonl), e.g. the recorded run.
    baseline: PathBuf,
    /// Candidate sidecar, e.g. the same audio through a changed engine.
    candidate: PathBuf,
},
```

Implementation (`cmd_diff_features`, delegating metric logic to
`audio_trace.rs`):

1. Load both sidecars: read each line, `serde_json::from_str::<SidecarHop>`,
   collect to `Vec<SidecarHop>`.
2. **Align on `hop`.** Both files should start at hop 0 and be contiguous. If
   their lengths differ, diff over the common prefix `min(a.len(), b.len())` and
   **report the length mismatch as a clear leading line**, e.g.
   `WARNING: length mismatch: baseline=N candidate=M, diffing common prefix=K`.
   Do not silently truncate without saying so.
3. Compute and print a report (to stdout) with, for the `f0_hz` series of each
   side:
   - hop count (baseline, candidate)
   - octave jumps (baseline, candidate) — reuse the same logic as
     `Captured::octave_jumps`: voiced-to-voiced transitions where
     `|1200*log2(hz/prev)| > 600`. **Factor this into a free function in
     `audio_trace.rs`** taking `&[f32]`, and have `Captured::octave_jumps` call
     it too (so there is one implementation). Likewise `per_hop`-style helpers
     are not needed here since the sidecar is already one-value-per-hop.
   - median voiced jitter cents (baseline, candidate)
   - voiced coverage fraction (baseline, candidate)
   - count of hops where `f0_hz` differs by more than 1 cent between baseline
     and candidate (over the aligned prefix), and the max divergence in cents.
4. Exit code: `0` always (this is a report, not a pass/fail gate — the human or
   a future test decides). Print the report in a clear, greppable table-ish
   text format.

## Refactor: single octave-jump implementation

`Captured::octave_jumps` (in `lib.rs`) currently inlines the metric. Extract the
core to `audio_trace.rs`:

```rust
/// Count voiced-to-voiced hop transitions whose pitch jumps more than
/// `threshold_cents`. The canonical octave-error metric, shared by the bench
/// `Captured` helper and the sidecar diff.
pub fn count_octave_jumps(hops_hz: &[f32], threshold_cents: f32) -> usize { ... }
```

Then `Captured::octave_jumps` becomes
`count_octave_jumps(&self.per_hop(path), 600.0)`. One implementation, two
callers.

**Do the same for jitter — this extraction IS clean, do not skip it.** Extract
`pub fn median_jitter_cents_of(hops_hz: &[f32]) -> f32` to `audio_trace.rs`
(the body of `Captured::median_jitter_cents` operates on the `per_hop` vec, so
it lifts directly). Have `Captured::median_jitter_cents` call it, and the diff
call it too. One implementation, two callers — same as octave jumps. Likewise
extract `pub fn coverage_voiced_of(hops_hz: &[f32]) -> f32` if the diff needs
it (it does, per step 3).

## Tests (in `dsp/bench`, run with `cargo test --release`)

Put unit tests for `audio_trace.rs` in that file's `#[cfg(test)] mod tests`.
Put any CLI-level test in `dsp/bench/tests/`.

Required tests:

1. **Sidecar round-trips.** Build a `Vec<SidecarHop>`, serialize to a string,
   parse back, assert equality.
2. **Manifest round-trips.** Same for `Manifest`.
3. **`count_octave_jumps` agrees with `Captured::octave_jumps`.** Construct a
   small hz series with a known octave jump (e.g. `[220, 220, 440, 220]` →
   2 jumps), assert both paths return the same count.
4. **Partial-block policy (THE KEY TEST).** Generate a synthetic sample buffer
   whose length is **deliberately NOT a multiple of 512** — e.g.
   `513`, `1025`, or `2048 + 200` samples of a 440 Hz sine. Run the
   replay-audio logic (factor the core of `cmd_replay_audio` into a testable
   function in `audio_trace.rs` that takes samples + config and returns
   `(Vec<SidecarHop>, Manifest)`, so no file I/O is needed in the test). Assert:
   - `n_hops == samples.len() / 512` (floor)
   - `manifest.total_samples == n_hops * 512`
   - the sidecar has exactly `n_hops` lines
   - the dropped tail (`samples.len() % 512` samples) did NOT create an extra
     hop.
   This locks the format contract before the recorder exists to honor it.
5. **End-to-end on the existing fixture.** `dsp/bench/test_data/` already
   contains `sa-re-ga-ma-pa.wav` — **48000 Hz, mono, int16, 376768 frames →
   736 hops** (confirmed; it exercises `read_wav_mono`'s Int branch). Mount
   `coach.json`, replay this file, and assert the sidecar has exactly 736 hops
   and a meaningful voiced fraction (e.g. `> 0.3`). Do NOT generate a sine or
   add a new binary fixture — use this one.

## Conventions (must follow)

- `cargo fmt`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace
  --release` must all pass clean (see root `CLAUDE.md`). The DSP sweeps are slow
  in debug — always use `--release` for tests.
- Match the existing `main.rs` style: `cmd_*` functions returning
  `anyhow::Result<()>`, errors via `.context(...)`, dispatch in the `match` in
  `main()`.
- No new per-directory README. Module doc comments (`//!`) are the
  documentation surface — write a clear one for `audio_trace.rs`.
- Keep `audio_trace.rs` to one cohesive responsibility: the sidecar/manifest
  format + the shared metric. CLI wiring stays in `main.rs`.

## Out of scope for Phase 1 (do NOT build)

- The recorder / any change to `domain-adapters/app-coach/` (that is Phase 2).
- The WAV `AudioCapture` adapter / `coach-game --replay-audio` (Phase 3).
- Resampling (WAV must already be the target rate — assert, as `read_wav_mono`
  does).
- A pass/fail threshold gate on the diff (report only).
- Compression/gzip of the sidecar (plain JSONL; revisit only if size bites).

## Done when

- `cargo run -p dsp-bench -- replay-audio <some.wav>` writes a
  `.features.jsonl` + `.manifest.json`.
- `cargo run -p dsp-bench -- diff-features a.features.jsonl b.features.jsonl`
  prints a report.
- All tests pass under `--release`; fmt + clippy clean.
- The partial-block test (#4) proves the floor-division contract.
