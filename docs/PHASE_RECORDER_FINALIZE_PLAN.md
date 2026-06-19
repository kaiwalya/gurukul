# Plan: trace artifacts survive an iOS kill (periodic flush, no exit event)

**Problem (root cause, verified):** the recorder finalizes the WAV header
(patching the RIFF + `data` chunk sizes from placeholder zeros) only in
`wav.finalize()`, which runs when the writer thread sees the channel close —
i.e. on `Drop` / clean session stop. **On iOS neither happens:**

- No app "quit" — iOS SIGKILLs the process on suspend/swipe; `Drop` never runs.
- No way to *leave* Free Practice yet — the session never stops either.

So on the sim the WAV header stays at zeros (`RIFF 0000 0000 … data 0000 0000`),
and every reader sees **0 frames** even though the samples are on disk. Same
root cause leaves the telemetry `-log.jsonl` empty and the manifest absent.

**The fix shape (user-approved):** don't depend on *any* end-of-process event
— iOS gives none. Instead make each artifact **valid-on-disk continuously**,
by flushing/patching as data is written. A SIGKILL then leaves a complete,
replayable file up to the last flush.

## Verified facts

| Fact | Where |
| --- | --- |
| WAV header patched only in `finalize()` | `audio_recorder.rs:157` (writer-thread channel-close arm) |
| `finalize()` reached only on channel close (Drop / `finish`) | `audio_recorder.rs:151-162`, `join_writer` 220+ |
| `hound` 3.5.1 has `WavWriter::flush()` — patches the header for a seekable sink, tested as "produces a valid file" | hound `src/lib.rs:715` `flush_should_produce_valid_file` |
| Our WAV target is a real file (seekable) | `audio_recorder.rs:93` `WavWriter::create(&wav_path, …)` |
| Sidecar already flushes on channel close | `audio_recorder.rs:153` |
| Telemetry log file is buffered, flushed only on Drop | `telemetry-std/src/lib.rs` (the new file sink) |

## Changes

### 1. `audio_recorder.rs` — periodic WAV flush in the writer loop

In the writer thread's `WriterMsg::Block` arm, after writing samples, call
`wav.flush()` on an interval so the header is continuously patched.

- **Interval:** flush every N blocks where N ≈ 1 second of audio. Blocks are
  512 samples at 48 kHz ⇒ ~94 blocks/s; flush every **94 blocks** (~1 s). Cheap:
  `flush()` seeks + rewrites only the ~8 header size-bytes, not the whole file.
- **Why not the infinite-file header (`0xFFFFFFFF` sizes)?** Verified our own
  replay reader (`hound` 3.5.1) computes `num_samples` directly from the data
  chunk size (`read.rs:600`) and errors if it's not an exact multiple — it has
  no read-to-EOF fallback. A `0xFFFFFFFF` size would make it try to read ~4 GB
  from a small file and fail. So the size field MUST be real; periodic flush is
  forced by the reader, not a stylistic choice.
- Keep the existing `finalize()` on channel close (the clean Mac path) — flush
  is additive, not a replacement. (`finalize()` after flushes is fine.)
- Count blocks written in the thread; `flush()` returns `io::Result` — on error,
  log + `invalid.store(true)` like the existing write-error handling (do NOT
  panic; a recorder must never take down the worker — `audio_recorder.rs:189`).
- A flush failure should NOT abort recording — best-effort, matching the
  module's non-blocking contract.

### 2. Telemetry log file — periodic flush

The new `-log.jsonl` file sink (`telemetry-std`) is a `BufWriter<File>` flushed
only on Drop, so an iOS kill leaves it empty (same root cause). Make `log()` /
`event()` **flush the file after each write** (the file sink is for debugging,
not a hot path; line-flush is acceptable and guarantees a killed run's log is
intact). Stderr is already effectively line-buffered. Do NOT change the stderr
path's behavior.

### 3. (Confirm) sidecar + manifest

- Sidecar (`.features.jsonl`) already flushes on channel close; add the same
  periodic flush alongside the WAV flush if cheap, so features survive a kill
  too. (One flush call per interval.)
- Manifest is written only on a *clean* `finish()` with a valid run — leave as
  is. A SIGKILL'd run legitimately has no manifest; the WAV+sidecar+log being
  valid is the win. (Do not try to write the manifest incrementally.)

## Explicitly NOT in this slice

- No iOS lifecycle observer (background/suspend hook) — fragile, limited time
  on suspend, and the periodic-flush approach makes it unnecessary.
- No "leave Free Practice" / session-stop UI — that's a separate feature; this
  fix must work *without* it (and does).
- No change to the Mac clean-exit path (Drop → finalize stays).
- No `into_header_for_infinite_file` (max-size header) approach — periodic
  `flush()` gives real sizes and is the tested path; the infinite-file header
  writes `0xFFFFFFFF` sizes (non-standard, some readers balk).

## Verify

1. `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
   `cargo test --workspace --release` clean.
2. **Unit/integration:** record a few blocks, then *without* calling `finish()`
   (simulating a kill) read the WAV file back and assert it has the expected
   non-zero frame count (i.e. a flush patched the header). This is the
   regression guard that the iOS-kill case now produces a valid file.
3. **Live sim proof:** run on the sim, sing ~5 s, then **hard-kill** the app
   (`simctl terminate`) WITHOUT a clean exit; pull the WAV off disk and confirm
   it is a valid, non-zero-frame file that `--replay-audio` loads on Mac (no
   hand-patching). Confirm `-log.jsonl` is non-empty too.
