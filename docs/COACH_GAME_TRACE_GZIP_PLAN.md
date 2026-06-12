# Plan: gzip the UX trace files

## Problem

Every `cargo run -p coach-game` writes a UX trace to
`traces/<stamp>/ux.jsonl` — one JSON object per line, flushed every frame.
These files are **large** (verbose per-frame `geom`/`coach`/`input`
records). They cost disk, are slow to move around, and bloat anything that
reads them whole. Both humans and agents debug from them (jq over the
channels; see `CONTRIBUTING.md`), so size hurts the core workflow.

## Goal

Store traces gzip-compressed, transparently on both the write and read
paths, while preserving:

1. **The per-frame crash-safety contract.** Today a killed run leaves an
   intact trace because the writer flushes every frame. This must survive
   compression.
2. **The replay round-trip contract.** `--replay` re-runs a trace and the
   `geom` channel comes out bit-for-bit identical
   (`tests/trace_replay_roundtrip.rs`). Compression must not perturb this.
3. **Easy ad-hoc reading** for humans and agents (the jq/grep workflow).

## Decisions (already made with the user)

| Question | Decision |
|----------|----------|
| File format | **`ux.jsonl.gz`, always gzipped.** One filename, one code path. No plain/compressed toggle, no magic-byte sniffing. |
| Read ergonomics | **Document `gzcat … \| jq` (or `gunzip -c`).** No helper script, no `--cat` flag. The standard tool already exists and agents know it. |

## Changes

### 1. Dependency: `flate2`

- Add `flate2 = "1"` to `[workspace.dependencies]` in the root `Cargo.toml`.
- Add `flate2 = { workspace = true }` to `apps/coach-game/Cargo.toml`.

`flate2` is the standard Rust gzip crate; default backend (`miniz_oxide`)
is pure Rust, no system zlib needed.

### 2. Writer — `src/trace/writer.rs`

Today: `BufWriter<File>` over `ux.jsonl`, `writeln!` per record,
`flush()` per frame.

Change the file to `ux.jsonl.gz` and wrap the file in a gzip encoder. The
layering matters for crash-safety:

```
GzEncoder<BufWriter<File>>      // compress, then buffer, then file
```

- `out.flush()` on a `GzEncoder` performs a `Z_SYNC_FLUSH`: it emits a
  complete, decodable deflate block and pushes it through to the file
  **without ending the gzip stream**. So the per-frame `flush()` keeps its
  meaning — every line up to the last flushed frame is pushed through
  userspace buffers and readable. (Like today's `BufWriter::flush`, this is
  not an `fsync`; it protects against app crash / kill, not power loss.)
- Compression level: `Compression::fast()`. JSONL compresses heavily
  regardless, and `fast` keeps the per-frame flush cheap.
- **Ratio under per-frame flush is fine.** `Z_SYNC_FLUSH` forces a block
  boundary + sync marker each frame but does **not** reset the compression
  dictionary — so the ratio hit is small (nothing like "a new gzip stream
  per frame"). Keep per-frame flush; no need to weaken contract (1). Spot-
  measure one real run before/after only if curious.

### 2a. Crash recovery on the READ side — the subtlety codex caught

A process killed mid-run leaves a gzip stream whose flushed blocks all
decode, but with **no final CRC/ISIZE trailer**. Empirically confirmed:
`MultiGzDecoder::read_to_string` on such a stream **returns an
`UnexpectedEof` error** — *even though it has already written every
flushed line into the output buffer*. So a naive
`read_to_string(...)?` would propagate that error and **refuse a crashed
run's trace** — defeating the entire crash-safety purpose.

The reader must therefore be **truncation-tolerant**:

- Read the decoded bytes incrementally (or `read_to_string`/`read_to_end`
  and **keep what was decoded so far on an `UnexpectedEof`**, rather than
  `?`-propagating it). Treat `UnexpectedEof` as "stream was truncated mid-
  flush — recover the prefix", not as a hard failure.
- Only the final partial line (bytes after the last flush) is lost — the
  existing `text.lines().filter(non-empty)` parse already drops a trailing
  partial line cleanly, but be sure a half-written final line can't parse
  as a valid record. Same loss boundary as today's BufWriter.
- Use `MultiGzDecoder` for its real purpose (tolerating concatenated
  members), but **do not** cite it as the recovery mechanism — the explicit
  EOF-tolerance above is the recovery mechanism.
- A truncation test is required (see Verification): write a sync-flushed
  trace, drop the trailer, assert all flushed lines still load & parse.

`create()` and `dir()` signatures stay the same; only the filename and the
wrapped writer type change.

**Finalize on graceful exit (added after smoke-testing).** Sync-flush alone
never writes the gzip *trailer*, so *every* trace — even a cleanly-closed
one — was trailerless. Stock `gzcat`/`gunzip` on **macOS** then emit
**nothing** (not "the recovered prefix" — that's GNU/Linux behaviour; BSD
refuses outright). That silently breaks the documented `gzcat | jq`
workflow for the normal case. Fix: a `finish()` method consumes the encoder
to write the trailer, wired to run on `AppExit` (window close / Cmd-Q) via a
`finish_writer` system in `Last`, after `flush_writer`. A graceful exit thus
yields a fully-valid `.gz`; only a hard crash / `kill -9` leaves a
trailerless stream, which the tolerant reader (and the documented `python3
zlib` one-liner / `--replay`) still recover. Proven by
`graceful_exit_writes_a_valid_gzip_trailer` (strict `GzDecoder` reads it
whole).

### 3. Reader — `src/trace/replay/load.rs`

Today: `fs::read_to_string(dir/ux.jsonl)` then `text.lines()`.

Change to open `dir/ux.jsonl.gz`, wrap in `MultiGzDecoder`, decode to a
`String` **tolerating `UnexpectedEof`** (per §2a), then the existing
`.lines()` parsing is unchanged.

- Keep the path in the error message (the existing nicety).
- The `newest_dir` logic is unaffected (it globs directories, not files).
- Test helpers in this file write `ux.jsonl` plain text — they must now
  write `ux.jsonl.gz` (gzip the `lines.join("\n")` before `fs::write`).

### 3b. Integration tests that read the trace directly

Three tests read `run/ux.jsonl` with `fs::read_to_string` and parse the
JSONL by hand — they must now read `ux.jsonl.gz` and decode it (a small
shared helper, e.g. `tests/common`, that decodes a run dir's trace to a
`String`):

- `tests/trace_recorder.rs:81`
- `tests/trace_recorder_layout.rs:75`
- `tests/trace_replay_roundtrip.rs:66` — this is the **bit-for-bit `geom`
  round-trip** test; it must decode *both* the original and the replay
  trace, so the determinism contract stays meaningful (codex's point).

### 4. Docs

- **`apps/coach-game/CLAUDE.md`** — the line documenting
  `traces/<…>/ux.jsonl` → `ux.jsonl.gz`.
- **`CONTRIBUTING.md`** — the "Debugging live runs from the trace" jq
  examples gain a `gzcat` (or `gunzip -c`) prefix, e.g.:

  ```sh
  gzcat traces/<dir>/ux.jsonl.gz | jq -r 'select(.k=="geom") | …'
  ```

  and the diff-the-replay example pipes both sides through `gzcat`. Note
  for the docs: a **killed** run's `.gz` has no trailer, so `gzcat` streams
  all recovered lines but may print an EOF warning and exit nonzero —
  harmless, but call it out so `set -o pipefail` users aren't surprised.
- **`AGENTS.md`** — wherever it states the trace file location/mechanics,
  update the name and add the one-line "unpack with `gzcat`" note.
- **`src/trace/record.rs`, `src/trace/mod.rs`, `src/trace/replay/mod.rs`,
  `src/lib.rs`, `ARCHITECTURE.md`** — module docs / comments: the
  `ux.jsonl` references become `ux.jsonl.gz`.

## Out of scope / explicitly deferred

- No helper script or `--cat` binary flag (decided against).
- No format toggle (plain vs gz) — always gz.
- No schema-version bump: the **record shape** is unchanged; only the
  container (gzip) changes. A reader pointed at an old plain `ux.jsonl`
  simply won't find `ux.jsonl.gz` — acceptable, traces are gitignored and
  ephemeral.
- No re-compression of existing on-disk traces (there are none committed).

## Verification

- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
  `cargo test --workspace --release` clean (project rule).
- `tests/trace_replay_roundtrip.rs` still passes — proves the gzip
  round-trip preserves the bit-for-bit `geom` contract.
- **New truncation test** (in `load.rs` or `tests`): sync-flush a few
  records, drop the gzip trailer, assert the loader still returns all
  flushed records — the crash-recovery contract in executable form.
- Manual: `cargo run -p coach-game`, confirm `ux.jsonl.gz` is written and
  `gzcat … | jq` reads it; kill mid-run and confirm `gzcat` still decodes
  the flushed prefix.
