# Plan: trace capture on the iOS simulator (retrievable bundle)

**Goal:** a run on the iOS sim writes its existing trace bundle to a place
I can pull off disk, closing the loop: user hits a bug on the sim → I read
the bundle → I diagnose (and later replay). **Capture only this slice**;
replaying sim recordings is deferred (per user's cut).

## What already exists (verified — NOT new work)

A live run already produces a 4-file bundle sharing one stamp, written by
`game/mod.rs` (audio) + `TracePlugin` (events):

| File | Content |
| --- | --- |
| `<stamp>-engine-input.wav` | raw mic audio (pre-engine) |
| `<stamp>-engine-input.features.jsonl` | engine features |
| `<stamp>-engine-input.manifest.json` | sidecar manifest pairing them |
| `<stamp>-ux.jsonl.gz` | UX/event trace |

Both replay modes exist too (`--replay` events, `--replay-audio` WAV).
None of this is the task.

## The one actual gap (verified on disk)

All three write sites build the path as **`PathBuf::from(trace::ROOT)`**
where `ROOT = "traces"` — a **relative** literal:

- `apps/coach-game/src/main.rs:155` (live `TracePlugin.root`)
- `apps/coach-game/src/main.rs:185/248` (replay `root`)
- `apps/coach-game/src/game/mod.rs:156` (engine-input WAV prefix)

On Mac, relative `traces/` resolves to the repo dir. On the **sim**, the
app's working directory is not writable (iOS sandbox), so
`TraceWriter::create` hits the error branch (`trace/mod.rs:~113`: log +
`return`) and the WAV writer fails the same way — **silently, no bundle**.

Confirmed by inspecting the device on disk
(`~/Library/Developer/CoreSimulator/Devices/<UDID>/data/Containers/Data/
Application/<app-uuid>/`): the app's data container exists (it ran), but
there is **zero `.wav` / `-ux.jsonl.gz`** anywhere — the writes went
nowhere.

## The change

| Where | Change |
| --- | --- |
| `trace/paths.rs` — new `pub fn trace_root() -> PathBuf` | resolve the trace root **per-OS**: iOS → `<sandbox-home>/Documents/traces`; everything else → relative `"traces"` (today's behavior). **Re-export it from `trace/mod.rs`** (paths.rs is private) so `main.rs`/`game/mod.rs` can call it. |
| the 3 call sites (main.rs:155, 185/248; game/mod.rs:156) | call `trace_root()` instead of `PathBuf::from(ROOT)` directly. |

**iOS root resolution (Codex-refined):** use **`NSHomeDirectory()`** — the
platform API — not `$HOME` (env-dependent). Returns the container root;
append **`Documents/traces`** (not bare `Documents`, to keep a clean trace
subdir). Add the iOS-gated Foundation dep to **`apps/coach-game/Cargo.toml`**
(coach-game makes the call; the dep can't live only in
`adapter-audio-cpal`). On disk this maps to
`…/Data/Application/<uuid>/Documents/traces/` — readable by UDID after the
sim shuts down.

Keep `ROOT = "traces"` as the non-iOS default so Mac/CLI behavior and all
existing tests are unchanged.

## Retrieval (docs, not code)

Add a short recipe to `apps/coach-game/BUILD.md`: resolve the data
container **while booted** with `xcrun simctl get_app_container booted
com.gurukul.coach-game data`, then read `Documents/traces/*` from that
path — it stays valid after shutdown. (The container UUID can change on
reinstall/erase, so resolve it per session rather than hard-coding;
shutdown alone preserves it.)

## Verify

1. fmt / clippy / `cargo test --workspace --release` clean (the per-OS fn
   must not change Mac behavior — existing trace tests stay green).
2. iOS sim compiles.
3. **Live proof:** run on the sim, do a short Free Practice session, shut
   the sim down, then read the bundle off the on-disk container path —
   confirm all 4 files (wav + features + manifest + ux) are present and
   the WAV is non-trivial.

## Explicitly NOT in this slice

Replaying sim-captured events (`--replay`) or audio (`--replay-audio`)
against the new bundles; any change to the bundle *format* or the capture
path itself; auto-pulling the bundle (manual recipe is enough for now).
