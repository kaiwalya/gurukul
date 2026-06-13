# Phase 3 spec — visual replay (WAV capture adapter + `coach-game --replay-audio`)

> For the code-editor. Implements the **visual replay** half of
> [`AUDIO_TRACE_PLAN.md`](AUDIO_TRACE_PLAN.md). Phases 1 (headless replay+diff)
> and 2 (recorder) are shipped. This phase lets you run the **real coach-game
> app** against a recorded WAV instead of the live mic, and watch the UI — the
> eyes-on path the user values most for confirming a future YIN fix.

## Goal in one sentence

`cargo run -p coach-game -- --replay-audio <file.wav>` boots the normal app
(real engine, real worker, real UI) but feeds it the WAV instead of the
microphone, real-time paced so the UI animates as if live.

## Why this is NOT like `--replay`

`coach-game --replay` (the existing UX-trace replay) bypasses the adapters and
the DSP engine entirely — it re-runs recorded UI inputs + coach reads. That is
the wrong tool for YIN: the engine never runs, so a changed engine would have
no effect.

`--replay-audio` is the opposite: **everything runs for real except the audio
source.** The cpal microphone adapter is swapped for a WAV-backed adapter. The
engine, the data-plane worker (and therefore the Phase-2 recorder too, if its
env var is set), the features, and the UI all run exactly as in a live session.

## Background — the two ports being faked

A live coach wires two audio ports (see `apps/coach-game/src/coach.rs`,
`build_coach`):

- **`AudioDevices`** — enumerates input devices; the UI lists them and the user
  picks one. `adapter_audio_cpal::new_devices()`.
- **`AudioCapture`** — opens a chosen stream and delivers `f32` PCM frames to a
  callback until the returned `CaptureSession` (RAII) is dropped.
  `adapter_audio_cpal::new_capture(clock)`.

Read both port docs before coding:
- `/Users/k/dev/gurukul/domain-ports/src/audio_capture.rs`
- `/Users/k/dev/gurukul/domain-ports/src/audio_devices.rs`

Key contract facts:
- `AudioCapture::open(handle, cfg, on_frame) -> Result<CaptureSession, _>`. The
  adapter picks its own thread; the callback is invoked **sequentially**.
  Frames are `f32` PCM in `[-1,1]`, **interleaved** if multi-channel.
- `CaptureSession` is **`!Send`** and stops the stream on drop (RAII, no
  explicit stop). Our teardown must signal the feeder thread to stop and join
  it on the dropping thread.
- `StreamHandle(pub Arc<dyn Any + Send + Sync>)` is opaque; each adapter
  stashes its own type inside and downcasts in `open`. A handle from a
  *different* adapter instance → `CaptureError::InvalidHandle`.
- `CaptureConfig { sample_rate, channels, buffer_frames }`. The host computes
  `buffer_frames = sample_rate/100` by default (~10ms) — see
  `control_plane.rs` `do_start_session` (~lines 216–226).

> ### ⚠️ THE central correctness rule (all 3 reviewers flagged this)
>
> **`CaptureConfig` is the source of truth at `open()`, NOT the WAV header.**
> The host derives `cfg.sample_rate` / `cfg.channels` *from the stream this
> adapter's `new_devices` vended*, then the data-plane worker downmixes
> assuming exactly `cfg.channels` interleaving
> (`control_plane.rs` `build_frame_callback(channels, …)`). So:
>
> 1. `new_devices` vends ONE stream whose `channels` / `sample_rate` **match the
>    WAV**.
> 2. In `open`, **validate the WAV against `cfg`**: require
>    `cfg.sample_rate == wav.sample_rate` and `cfg.channels == wav.channels`;
>    on mismatch return `CaptureError::UnsupportedConfig { reason }` — do NOT
>    silently proceed.
> 3. Drive **all** runtime math off `cfg`: chunk size, pacing duration, frame
>    count. Never recompute from the WAV header after validation.
> 4. Guard invalid configs: `cfg.sample_rate == 0`, `cfg.channels == 0`, or
>    `cfg.buffer_frames == Some(0)` → `UnsupportedConfig`.
>
> The mono float32 WAV that Phase 2 writes makes `cfg.channels == 1` line up
> naturally — but only because (1) vends a matching stream. Treating the WAV
> header as runtime truth would work by coincidence today and break the moment
> channels differ. Drive off `cfg`.

## What to build

### 1. New adapter crate `domain-adapters/audio-wav/`

Crate name `adapter-audio-wav` (matches the `<port>-<flavor>` convention in
`domain-adapters/CLAUDE.md`; flavor `-wav`). It implements BOTH ports (like
cpal's crate does), exposing two factories mirroring cpal exactly:

- `pub fn new_devices(wav_path: PathBuf) -> impl AudioDevices`
- `pub fn new_capture(clock: Arc<dyn Clock>) -> impl AudioCapture`

`Cargo.toml` deps: `domain-ports = { path = "../../domain-ports" }`,
`hound = "3.5"`, and `domain-ports`'s `Clock` (already in domain-ports).

#### 1a. `devices.rs` — the fake `AudioDevices`

Vends exactly ONE device with ONE stream representing the WAV file, so the
coach-game device-selection UI has something to list and pick. Read the WAV
header (via `hound::WavReader::open(...).spec()`) once in `new_devices` to
report the file's real `sample_rate` and `channels`.

- `list_devices()` → a `Vec` with one `InputDevice`:
  - `persistent_id: Some(DeviceId("wav-replay".into()))`
  - `name: "WAV replay: <basename>"`
  - `transport: Transport::Virtual`
  - one `InputStream` (below)
- `default_input()` → `Some(<that stream>)`
- The `InputStream`:
  - `handle: StreamHandle(Arc::new(WavStreamHandle { path, spec }))` — a private
    handle struct holding the WAV path + parsed `hound::WavSpec`. `WavStreamHandle`
    must be `Send + Sync` (PathBuf + WavSpec are).
  - `name: "WAV replay"`, `channels: <from spec>`,
    `sample_rates: SampleRateSupport::List(vec![<spec.sample_rate>])`.

`InputDevice`/`InputStream` aren't `Clone`-friendly across the board
(`StreamHandle` is `Clone`; the structs derive `Clone`). Re-mint on each
`list_devices` call by rebuilding from the stored path+spec, OR clone the
stored stream — either is fine; cpal's adapter rebuilds.

#### 1b. `capture.rs` — the real-time-paced `AudioCapture`

`new_capture(clock)` returns a `WavAudioCapture { clock }`. `open`:

1. Downcast `handle.0` to `WavStreamHandle`; on failure
   `Err(CaptureError::InvalidHandle)`. (`devices.rs` and `capture.rs` MUST
   share the same private `WavStreamHandle` type, or the downcast always fails.)
2. **Validate `cfg` against the WAV header** (the central rule above):
   `cfg.sample_rate == wav.sample_rate`, `cfg.channels == wav.channels`,
   and none of `cfg.sample_rate` / `cfg.channels` is 0, `cfg.buffer_frames !=
   Some(0)`. Any failure → `Err(CaptureError::UnsupportedConfig { reason })`.
3. Read ALL samples from the WAV into a `Vec<f32>` up front (these recordings
   are short; no need to stream from disk). Use `hound::WavReader`; handle both
   `SampleFormat::Float` (read `f32`) and `Int` (normalize to `[-1,1]` by
   dividing by the max magnitude for the bit depth) — mirror dsp-bench's
   `read_wav_mono` int branch so behavior matches the headless path. The WAV we
   write in Phase 2 is always float32 mono, but supporting int keeps the adapter
   honest for hand-supplied files. **Reject** a sample count not divisible by
   `cfg.channels` → `UnsupportedConfig`.
4. Compute, all from `cfg`:
   - `channels = cfg.channels as usize`
   - `chunk_frames = cfg.buffer_frames.unwrap_or(cfg.sample_rate / 100) as usize`
     (in practice the host always passes `Some(sample_rate/100)`).
   - `chunk_samples = chunk_frames * channels`.
5. **Move** the samples `Vec` into a **feeder thread**
   (`thread::Builder::new().name("wav-replay-feeder")`). The `Vec` and the
   `on_frame` callback are `Send`, so they move in cleanly. The thread:
   - Walks the samples in `chunk_samples`-sized slices. The final slice may be
     shorter; its `frames = slice.len() / channels` (guaranteed whole by the
     divisibility check in step 3).
   - **Before** each chunk: if the `Arc<AtomicBool> stop` flag is set, exit.
   - Call `on_frame(CaptureFrame { samples: slice, frames, t_ms:
     clock.now_ms() })`.
   - Then pace: sleep the chunk's real duration
     (`frames as f64 / cfg.sample_rate as f64 * 1000.0` ms) **in small
     interruptible steps** (e.g. loop sleeping ≤2ms, breaking early if `stop`
     is set) so teardown doesn't wait out a whole chunk's sleep. Exact drift
     doesn't matter — visual only.
   - When samples are exhausted: `eprintln!` **one** drain line
     (`"[adapter-audio-wav] replay drained: <basename> ({n} frames, {secs}s)"`)
     so a CLI user isn't left wondering whether it ended or stalled. Then the
     thread ends. **Do NOT loop the WAV** (a loop-seam octave jump would look
     like a detector glitch — defeats the visual check).
6. Return `CaptureSession::new(move || { stop.store(true, Release); join the
   feeder })`. Joining on drop is correct and matches the RAII contract: the
   session is `!Send`, so it's dropped on the thread that opened it (coach
   control thread); the feeder is a separate Send thread we join there. No
   deadlock — the callback is synchronous and owned by the feeder, so the
   drop-thread and feeder never share a lock.

### 2. Wire `--replay-audio <wav>` into coach-game

In `apps/coach-game/src/main.rs`:

- Extend `parse_args`: add `replay_audio: Option<PathBuf>`. Parse
  `--replay-audio <path>` (path is REQUIRED here, unlike `--replay`; if missing
  or starts with `--`, print an error to stderr and `exit(1)`).
- **Reject the ambiguous combo.** If BOTH `--replay` and `--replay-audio` are
  given, print an error to stderr and `exit(1)` — they are different execution
  modes (bypass-engine vs swap-mic) and silent precedence is a footgun.
- `--replay-audio` is a **live run variant**, not a replay-trace run. In `main`,
  route: if `args.replay.is_some()` → `run_replay` (unchanged); else →
  `run_live(args.replay_audio)`.
- `run_live` takes `replay_audio: Option<PathBuf>` and passes it to a new
  `coach::build_coach_with_audio(replay_audio)`:
  - `None` → today's behavior (cpal devices + capture).
  - `Some(wav)` → `adapter_audio_wav::new_devices(wav)` +
    `adapter_audio_wav::new_capture(clock)` in place of the two cpal factories.
    Everything else in `build_coach` is unchanged.
  - **Shared clock:** `build_coach` already builds one `Arc<dyn Clock>` at the
    top (`coach.rs` ~line 67) and hands it to telemetry + `AppCoachDeps`. The
    WAV capture's `new_capture(clock)` MUST receive a clone of that SAME clock,
    so audio `t_ms` shares an epoch with the rest of the session. Do not mint a
    second clock.
  - Keep the existing `build_coach()` as a thin wrapper calling
    `build_coach_with_audio(None)` so other callers (tests) are untouched.
- The UX trace still records normally (a `--replay-audio` run is a live run); its
  header `replay_of` stays `None`. That's fine — it's a real session.
- Update `main.rs`'s module doc to document `--replay-audio` next to `--replay`,
  making the distinction explicit (one swaps the mic, the other bypasses the
  engine).
- Add `adapter-audio-wav = { path = "../../domain-adapters/audio-wav" }` to
  `apps/coach-game/Cargo.toml`.

### 3. Workspace

Add `domain-adapters/audio-wav` to the root `Cargo.toml` workspace members.

## Tests

In the adapter crate (`domain-adapters/audio-wav/`):

1. **Devices report the WAV's format.** Write a tiny float32 WAV to a tempdir
   (`hound::WavWriter`, mono, 48k, a few hundred samples), build
   `new_devices(path)`, assert `list_devices()` has one device whose stream
   reports `channels == 1` and `sample_rates` listing `48000`, and
   `default_input()` is `Some`.
2. **Capture feeds all samples then stops.** Build `new_capture(clock)` (use
   `domain_ports::clock::TestClock` — it's in the `test-util` feature; add
   `domain-ports` with `features=["test-util"]` as a dev-dependency), open with
   the device's handle + a `CaptureConfig`, and a callback that accumulates the
   total frame count into a shared counter. Sleep enough for the (short) WAV to
   drain, drop the session, and assert the accumulated sample count equals the
   WAV's sample count. (Pace will be near-instant for a few-hundred-sample WAV.)
3. **Wrong handle → InvalidHandle.** Open the capture with a
   `StreamHandle(Arc::new(()))` (a foreign handle) and assert
   `Err(CaptureError::InvalidHandle)`.
4. **Config mismatch → UnsupportedConfig.** Open with the right handle but a
   `CaptureConfig` whose `sample_rate` (or `channels`) differs from the WAV;
   assert `Err(CaptureError::UnsupportedConfig { .. })`.

Use `tempfile` as a dev-dependency.

### CLI test (in coach-game)

If `parse_args` is unit-testable (extract the arg-vec parsing into a helper that
takes `impl Iterator<Item = String>` so it doesn't read the real `std::env`),
add a test that passing BOTH `--replay` and `--replay-audio` is rejected. If
making it testable is disproportionate, a manual step-5 check suffices — note
which you chose.

## Constraints / non-goals

- **Visual only, NOT bit-exact.** The ring + real-time pacing re-enter here, so
  this path can drop/realign samples under load — that is acceptable and
  expected. The bit-exact contract belongs to the Phase-1 headless path. Say so
  in the adapter's module doc.
- **Do not loop the WAV, no scrubbing, no speed control** (deferred per the
  plan).
- **Do not change the `AudioCapture` / `AudioDevices` port traits.** The WAV
  adapter conforms to them as-is.
- **Do not modify the cpal adapter.** The WAV adapter is a sibling.
- **Do not add the YIN fix.** Infrastructure only.
- Keep `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, and
  `cargo test --workspace --release` clean. Prefer `is_multiple_of(n)` over
  `% n == 0`.

## End-of-replay behavior (SETTLED — UI-designer + architect + Codex agreed)

When the WAV ends, **just stop feeding** — the UI freezes on the last features
then goes quiet. Do NOT add an end-of-stream signal to the port (it models a
never-ending mic; an EOS concept is disproportionate for a dev-only visual
check and would touch the contract). Do NOT loop (a loop-seam octave jump would
look like a detector glitch). The only affordance is the single `eprintln!`
drain line (step 5 above), so a CLI user can tell "clip ended" from "stalled."
The session stays `Running`; that's acceptable for Phase 3.
