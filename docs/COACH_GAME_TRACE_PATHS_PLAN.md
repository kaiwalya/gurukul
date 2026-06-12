# Plan: centralize trace path/name logic + flat-file layout

## Problem

Two issues, found right after the gzip change:

1. **The trace filename is scattered.** The literal `"ux.jsonl.gz"` is
   hardcoded at 4 sites (writer, loader, 2 test helpers). The trace *root*
   `"traces"` is hardcoded at 2 sites in `main.rs`. When the name changed
   (`ux.jsonl` → `ux.jsonl.gz`) every site needed a hand-edit — exactly the
   drift centralization prevents. Only the run-dir *stamp*
   (`launch_stamp()` in `wallclock.rs`) is already centralized; it is the
   model to follow.

2. **The on-disk layout is a directory-per-run**, which is heavier than
   needed. Today: `traces/<stamp>/ux.jsonl.gz` — one folder per run holding
   a single fixed-named file. We want a **flat** layout: the stamp moves
   into the filename and the file sits directly under `traces/`.

## Target layout (decided with the user)

```
traces/2026-06-12-002732-123-ux.jsonl.gz
        └──────┬──────┘ └┬┘ └────┬────┘
          date+time     millis   fixed suffix
```

- **Stamp:** `YYYY-MM-DD-HHMMSS-mmm` (compact time, no inner dashes; a
  3-digit millisecond field appended). Chosen for closeness to today's
  format. Still **lexicographically sortable** → newest sorts last (the
  invariant `newest_*` relies on). Millis added so two runs in the same
  second don't collide now that there's no directory to disambiguate.
- **Filename:** `<stamp>-ux.jsonl.gz`. The `-ux.jsonl.gz` suffix is the
  fixed part.
- **No per-run directory.** Files live directly under `traces/`.
- **Replay output:** a flat sibling with its own fresh stamp —
  `traces/<new-stamp>-ux.jsonl.gz`. The `replay_of` header field already
  links it back to the source trace, so dropping the folder loses no
  relationship. (Decided: "Flat sibling, new timestamp".)

## Design: one `trace::paths` module owns it all

New module `src/trace/paths.rs`, the single door for trace path concerns:

```rust
/// Gitignored directory all traces live under, relative to the working dir.
pub const ROOT: &str = "traces";

/// Fixed suffix of every trace file: `<stamp>-ux.jsonl.gz`.
const SUFFIX: &str = "-ux.jsonl.gz";

/// The file name for a run stamped `stamp`.
pub fn file_name(stamp: &str) -> String { format!("{stamp}{SUFFIX}") }

/// Full path for a run: `<root>/<stamp>-ux.jsonl.gz`.
pub fn file_path(root: &Path, stamp: &str) -> PathBuf { root.join(file_name(stamp)) }

/// The newest trace file under `root` (lexicographically greatest name —
/// stamps sort by construction), or `None` if there are none. Replaces
/// `newest_dir`; now matches *files* ending in SUFFIX, not subdirectories.
pub fn newest(root: &Path) -> Option<PathBuf> { … }
```

`ROOT` is re-exported so `main.rs` stops saying `PathBuf::from("traces")`.

## Changes by file

### `src/trace/wallclock.rs` — stamp gains millis
- `launch_stamp()` returns `run_stamp` = `YYYY-MM-DD-HHMMSS-mmm` (was
  `YYYY-MM-DD-HHMMSS`) and `wall_start` unchanged.
- Pull milliseconds from the same `SystemTime` (`duration.subsec_millis()`)
  — one `now()` read, so the stamp and header agree.
- Update the doc comment + the `run_dir_is_sortable_and_shaped` test
  (length 17 → 21; assert the millis field shape). Rename "run_dir" → the
  term used downstream ("stamp") in the doc, since it's no longer a dir.

### `src/trace/paths.rs` — new module (above). Add `mod paths;` +
re-exports in `src/trace/mod.rs`.

### `src/trace/writer.rs` — write the flat file
- `TraceWriter::create(root, stamp)` now creates
  `paths::file_path(root, stamp)` (still `create_dir_all(root)` first).
- **Collision safety (review finding, High).** With no per-run directory,
  two runs that stamp the same millisecond — or a backwards clock step —
  would land on the same filename, and `File::create` *truncates*,
  silently destroying the earlier trace. Open with
  `OpenOptions::new().write(true).create_new(true)`; on `AlreadyExists`,
  retry with a stable tie-breaker inserted before the suffix —
  `<stamp>.001-ux.jsonl.gz`, `.002`, … — so a trace is never overwritten.
  (A `paths::create_new_file(root, stamp)` helper owns this loop.) The
  separator is `.`, not `-`: `.` sorts *after* the primary's `-ux…` suffix
  but *before* the next millisecond's stamp, so `newest` still returns the
  most recent file of a colliding pair (a `-` separator inverts that — a
  bug the final review caught; covered now by `tiebreaker_sorts_after_primary`).
- `dir()` accessor: today it returns the run directory. Callers
  (`mod.rs` log line) use it only for a log message. Replace with a
  `path()` accessor returning the file path; update the one caller.

### `src/trace/mod.rs` — plugin fields
- `TracePlugin.run_dir: String` → `stamp: String` (rename; same type).
  Doc comment updated.
- Build path passes `stamp` through to `TraceWriter::create`.
- Re-export `paths` (and keep `launch_stamp`).

### `src/trace/replay/load.rs` — load by file path, newest by file
- `load(dir)` → `load(path)`: it now takes the **trace file path**
  directly (`traces/<stamp>-ux.jsonl.gz`), not a directory. Drop the
  internal `dir.join("ux.jsonl.gz")` — the `decode_gz(path)` call already
  takes a path.
- `newest_dir` → moves to `paths::newest` (file-matching). Update the
  `pub use` in `replay/mod.rs`.
- **Test helpers** (`write_trace`, `write_raw`, the gz-writing test
  helper) write `traces/<stamp>-ux.jsonl.gz` flat and return the **file
  path**; the bare `"ux.jsonl.gz"` literals are gone (use `paths`).
- `newest_dir_picks_greatest_name` test → `newest_picks_greatest_file`,
  asserting file selection.

### `src/main.rs` — flat paths, file-based replay
- `run_live`: `let (stamp, wall_start) = trace::launch_stamp();` →
  `TracePlugin { root: PathBuf::from(paths::ROOT), stamp, … }`.
- `run_replay`: `let root = PathBuf::from(paths::ROOT);`
  `explicit.or_else(|| paths::newest(&root))` yields a **file path** now.
  `replay::load::load(&src)` takes that file path.
- `Args.replay` doc + `--replay [path]`: the optional arg is now a **file
  path** (`traces/<stamp>-ux.jsonl.gz`), not a dir. Update the doc
  comment.
- `dir_name(&src)` → `file_label(&src)`: still `p.file_name()`, fine for a
  file; used for `replay_of` + the window title. (A file path's
  `file_name()` is the stamped filename — a good `replay_of` label, and
  directly pasteable as `--replay traces/<that>`.)
- **Old-layout hint (review finding).** Someone with a pre-change trace
  will type `--replay traces/<old-stamp>` (a *directory*); `load` would
  then surface a raw "Is a directory" OS error. Add a one-line guard in
  `run_replay`: if the resolved path `is_dir()`, print a hint that the
  layout is now a flat `traces/<stamp>-ux.jsonl.gz` file and exit. `newest`
  stays file-only (old dirs are correctly ignored).

### `tests/` — integration tests hardcode the old layout (review finding)
The signature/field changes ripple into the integration tests; list them so
they aren't discovered mid-edit:
- `tests/common/mod.rs` — `decode_trace` joins `ux.jsonl.gz` internally;
  change it to take the **trace file path** directly (callers pass
  `paths::file_path(root, "run")`), and drop the literal.
- `tests/trace_recorder.rs` — the `run_dir: "run"` plugin field → `stamp:
  "run"` (rename), and the runtime path `root.join("run").join("ux.jsonl.gz")`
  → `paths::file_path(&root, "run")`. With `stamp: "run"` the file is
  `traces-root/run-ux.jsonl.gz` — fine for a test.
- `tests/trace_recorder_layout.rs`, `tests/trace_replay_roundtrip.rs` —
  same `run_dir` → `stamp` field rename; the roundtrip test's `load`/decode
  calls move to file-path input.

### Docs
- `apps/coach-game/AGENTS.md` (note: `CLAUDE.md` is a **symlink** to it —
  one edit covers both), `CONTRIBUTING.md`: every `traces/<dir>/ux.jsonl.gz`
  → `traces/<stamp>-ux.jsonl.gz`; `--replay traces/<dir>` →
  `--replay traces/<file>`; the gzcat/python/jq examples updated.
- `apps/coach-game/ARCHITECTURE.md`, module docs in `trace/mod.rs`,
  `trace/record.rs`, `trace/replay/mod.rs`, `src/lib.rs`: path references.
- `.gitignore`: already ignores `traces/` wholesale — no change.

## Invariants to preserve (call out for review)

1. **Sortability:** `newest` must still pick the most recent run. Verified
   by the stamp being fixed-width and lexicographically ordered.
2. **Replay round-trip:** the bit-for-bit `geom` contract
   (`trace_replay_roundtrip.rs`) must still hold — only the *path* changes,
   not record bytes. The test reads via `common::decode_trace`, which moves
   to file-path input.
3. **Crash recovery:** the gzip truncation tolerance is untouched; its
   tests just write flat files now.
4. **No schema bump:** record shapes are unchanged. The container path
   changes; that is not a schema concern.

## Future-proofing note (review)
Flattening does **not** foreclose the one deferred feature that wanted a
per-run directory — screenshots (the `mark` record reserves a field; see
the deferred list in `trace/mod.rs`). Because the stamp is unique per run,
a sidecar lives fine as a flat prefix: `traces/<stamp>-mark-001.png`
alongside `traces/<stamp>-ux.jsonl.gz`. Don't reintroduce per-run
directories reflexively when that lands.

## Out of scope
- No change to record contents, gzip mechanics, or the finalize-on-exit
  logic (just shipped).
- **No migration of existing on-disk traces.** Old `traces/<stamp>/ux.jsonl.gz`
  *directories* are intentionally not discoverable by `--replay` (the new
  `newest` matches flat files only) and `--replay traces/<old-stamp>` (a
  dir) is rejected with the hint above. Traces are gitignored and
  ephemeral, so there's nothing to convert.

## Verification
- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
  `cargo test --workspace --release` clean.
- Updated stamp test (length/shape + millis).
- `newest` file-selection test.
- `trace_replay_roundtrip` still green (path change doesn't perturb geom).
- Manual: `cargo run -p coach-game`, confirm
  `traces/<stamp>-ux.jsonl.gz` appears; `--replay` (no arg) picks it;
  `--replay traces/<that-file>` works explicitly.
