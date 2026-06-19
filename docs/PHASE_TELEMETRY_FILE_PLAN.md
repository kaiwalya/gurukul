# Plan: telemetry logs as a retrievable file (tee stderr + `<prefix>-log.jsonl`)

**Goal:** the structured logs that today vanish on the iOS simulator (stderr
is swallowed by `simctl`) land as a **file in the traces folder**, sibling to
the existing wav/ux/features bundle, sharing the run stamp. This closes the
loop: a sim run's logs become retrievable off-disk like the rest of the trace.

**Shape (settled with the user, after architect + Codex review):** do NOT
promote trace to a port. Mirror the **audio-trace recorder pattern** instead —
that recorder (`domain-adapters/app-coach/src/audio_recorder.rs`) takes a
**path prefix at construction** and owns its files from there. Telemetry does
the same: the `telemetry-std` adapter gains an optional run-prefix; given one,
it **tees** every line to stderr AND `<prefix>-log.jsonl`. No new trait, no new
port — one optional arg on the existing factory.

## What exists (verified)

| Piece | Where |
| --- | --- |
| `telemetry-std` adapter: `StderrTelemetry { core, out: Arc<Mutex<io::Stderr>> }` | `domain-adapters/telemetry-std/src/lib.rs` |
| factories `new(clock)` / `with_context(clock, ctx)` → `impl Telemetry` | same, lines 32/42 |
| `log` / `event` write via `writeln!(out, …)`; `child` shares the `Arc` handle | same, 56–79 |
| telemetry built at boot | `coach.rs:89` (game) **and** `coach-cli/src/main.rs:71` |
| run stamp minted | `main.rs:152` (`trace::launch_stamp()`) — **one line AFTER** the coach builder runs (`main.rs:147`) |
| trace root (per-OS, iOS → Documents sandbox) | `trace::trace_root()` |
| audio-recorder prefix pattern to mirror | `audio_recorder.rs:60` `Recorder::new(prefix, …)` |

## The wiring problem + fix

The stamp is born **after** telemetry is constructed, so telemetry can't see
it today. Fix (user-approved): **mint the stamp first** in `main.rs`, thread it
into the coach builder, and on into the telemetry factory.

Bonus: `launch_stamp()` is currently called separately (main.rs:152 and :246),
so audio/ux stamps can differ. Minting once up front and threading it makes
**all recorders share one stamp**.

## Changes

### 1. `telemetry-std` adapter — optional file sink, tee

| Where | Change |
| --- | --- |
| `lib.rs` factories | add `log_path: Option<PathBuf>` param to `new` and `with_context`. When `Some`, open the file (`File::create`, buffered) and hold it; when `None`, behave exactly as today (stderr-only). |
| `StderrTelemetry` struct | add a second optional sink: `file: Option<Arc<Mutex<BufWriter<File>>>>`. Keep the existing `out: Arc<Mutex<io::Stderr>>` unchanged. Children clone BOTH `Arc`s. |
| `log` / `event` impls | after writing to stderr (unchanged), if `file.is_some()` write the **same rendered line** to the file too. Best-effort: a file write error must not panic or disturb stderr (swallow like the existing `let _ =`). |
| line format | the file gets the **identical** `[LEVEL] msg {…}` / `[EVENT] {…}` text the stderr path already produces — NOT a different JSON schema. (Filename says `.jsonl` only by convention with the bundle; keep the existing greppable line format. If a true JSON line is wanted later, that's a separate change — do NOT invent a schema now.) |

**Filename:** `<prefix>-log.jsonl` where `prefix` is the run stem
(`<root>/<stamp>`). Reuse the same prefix the audio/ux files derive from so all
four share the stamp. The adapter appends `-log.jsonl` (mirroring how
`Recorder` appends `.wav`).

**Failure policy (mirror the audio recorder):** if the file can't be created
(e.g. parent dir missing), log/skip the file sink and fall back to stderr-only
— never take down logging. Create the parent dir best-effort first
(`fs::create_dir_all`), exactly like `audio_recorder.rs:68`.

### 2. Thread the prefix to the builder

| Where | Change |
| --- | --- |
| `coach.rs` | `build_coach_with_audio(replay_audio, log_prefix: Option<PathBuf>)`; pass `log_prefix` into `adapter_telemetry_std::new(clock, log_prefix)`. Keep the `build_coach()` thin wrapper passing `None`. |
| `main.rs` | mint the stamp BEFORE building the coach: call `trace::launch_stamp()` first, compute `prefix = trace_root().join(&stamp)`, pass `Some(prefix)` into `build_coach_with_audio`, and reuse the SAME `(stamp, wall_start)` for `TracePlugin` (do not call `launch_stamp()` a second time for the live path). |
| `coach-cli/src/main.rs:71` | pass `None` → stderr-only, behavior unchanged. |

**Replay paths (main.rs ~185/246):** these already mint their own stamp; pass
`Some(prefix)` if a coach is built there too, else `None`. Keep replay behavior
otherwise untouched. (Confirm whether the replay branches build a coach with
telemetry; if they don't, leave them.)

## Explicitly NOT in this slice

- No new port / `TraceRecorder` trait (rejected by review — trace stays
  app-level; its Bevy/`FrameCount` surface is irreducibly head-specific).
- No neutral `RunRecording` struct extraction yet (deferred to "if a third
  consumer appears"). For now the prefix is threaded as a plain `PathBuf`.
- No JSON log schema — keep the existing greppable line format.
- No change to the audio-trace or ux-trace recorders.
- No gzip on the log file (the ux trace is gzipped; the log stays plain for
  live `tail`/`grep`).

## Verify

1. `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
   `cargo test --workspace --release` clean. The `None` path must leave every
   existing telemetry test green (stderr-only unchanged).
2. A unit test: `new(clock, Some(tmp_prefix))`, log a line + emit an event,
   assert `<tmp_prefix>-log.jsonl` contains both rendered lines AND the same
   lines still reach the stderr path (extend the existing `BufTelemetry`-style
   test, or add a file-sink test alongside it).
3. **Live sim proof:** run on the iOS sim, do a short session, shut down, read
   `<container>/Documents/traces/<stamp>-log.jsonl` off-disk — confirm it holds
   the run's log lines (the `build_input_stream` failure path should now be
   visible there, which was the original motivation).
