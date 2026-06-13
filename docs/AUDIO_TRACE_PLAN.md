# Plan: audio-thread trace + replay (engine-input flight recorder)

## Problem

YIN pitch detection produces **octave errors**: f0 jumps an octave between
hops. In a real session (`traces/2026-06-12-224654-338-ux.jsonl.gz`), ~22% of
voiced hops jump more than a semitone, and 77 of them land within a hair of a
full octave (1200 cents). The fine ±12-cent jitter is normal for YIN; the
octave jumps are the classic period-doubling/halving artifact and are the bug
to fix.

Before fixing, we want infrastructure to:

1. **Record** a real session's audio + the features it produced.
2. **Replay** that audio through a *changed* engine.
3. **Verify** the octave jitter is gone — two ways:
   - **Numeric / headless** — diff the f0 series ("77 octave jumps → 2"). For
     regression and CI.
   - **Visual** — run the real `coach-game` app against the recorded audio and
     watch the UI. The user values this eyes-on path most.

## Why the existing UX trace doesn't cover this

The UX trace (`apps/coach-game/src/trace/`) taps the **`AppCoach` port** — the
seam where the engine's `FeatureSnapshot` crosses into Bevy. That is the wrong
layer for YIN: by the time f0 reaches the app, the octave error already
happened. We must tap **one layer down**, at the audio/DSP seam, *upstream* of
YIN. This follows the UX trace's own doctrine — "record upstream of everything
you claim to replay."

## The seam (verified against the code)

```
cpal RT callback (audio thread)
  │ samples: &[f32], multichannel
  ▼
downmix to mono  ── control_plane.rs:423
  │
  ▼
SPSC ring buffer  ◄── DROPS ON OVERFLOW. "what the mic said" ≠ "what the engine ate"
  │
  ▼
"app-coach-data" worker thread:        ── data_plane.rs
  pops a full 512-sample block         ── :318  (partial final block never processed)
  engine.in_port(mic) = block          ── :344  ◄── TAP: engine-input samples
  engine.process_block(512)
  reads 6 ports → FeatureSnapshot       ── :357  ◄── TAP: features out
  feature_publisher.store (ArcSwap) + history ring
  │
  ▼
AppCoach::latest_features()/drain_features()  (app thread — what the UX trace sees)
```

### Terminology: frame vs block vs hop

Audio overloads "frame," so this plan is deliberate about it (and these are not
the Bevy *render* frame the UX trace's `f` field counts):

- **frame** — one sample across all channels at one instant (cpal's term; cf.
  `buffer_frames`, `BLOCK_FRAMES`, `CaptureFrame`). The atomic unit.
- **block** — the chunk of frames the worker pops at once: 512 frames here.
- **hop** — the analysis stride between successive YIN windows. The window is
  2048 frames; it advances one hop (512 frames) per pass.

In *this* pipeline block size == hop size == 512, so the three coincide and
`hop_index` also counts blocks — but they are distinct concepts (a 256-frame
hop would yield two analyses per 512-frame block). Each `FeatureSnapshot`
corresponds to one **hop**, so the feature sidecar is keyed by `hop_index`.

Key facts confirmed by both reviewers (architect + Codex):

- The engine is a **pure function of input samples** — no RNG, no wall-clock
  except `t_ms`. So sample-in → feature-out is reproducible.
- The ring **drops on overflow**, and downmix happens *before* the ring. A
  single dropped sample shifts every later block's alignment. Therefore we must
  record the **block the engine actually received**, post-ring/post-downmix —
  not the raw mic.
- The worker processes **only whole 512-sample blocks**; the partial final
  block is never processed.
- `dsp/bench` (`dsp_bench`) **already** drives a WAV through a world
  block-by-block (`dsp/bench/src/lib.rs:88`, `:253`). The headless harness
  factors out of this rather than starting fresh.

## Design decision (settled with the user + both reviewers)

My first instinct — *one* WAV-backed `AudioCapture` adapter as the single swap
point for both replays — was **rejected** by both reviewers. It forces one
mechanism to serve two timing regimes and records at the wrong layer (mic
output, not engine input).

**The shared thing is not an adapter. It is "recorded mono engine-input
samples + a manifest."** Two consumers sit on top:

```
        recorded artifact (one per session):
        ├─ <stamp>-engine-input.wav   (f32 mono, exactly the 512-blocks the engine ate)
        ├─ <stamp>-features.jsonl     (FeatureSnapshot per block)
        └─ <stamp>-manifest.json      (sample rate, block size, world hash, git SHA, …)
                       │
            ┌──────────┴───────────┐
            ▼                      ▼
   WAV → AudioCapture       direct block runner
   adapter (real-time       (no ring, no Bevy —
   paced)                    pure engine loop)
            │                      │
   coach-game               headless diff
   --replay-audio <wav>     (regression / CI)
   [watch the UI]           [77 → 2 octave jumps]
```

### Format decisions

- **Audio: float32 WAV.** Playable/listenable (the user wanted this), and int
  PCM quantization could mask or invent YIN edge cases. Named
  `*-engine-input.wav`, *not* `*-mic.wav` — it is what the engine heard, not
  what the mic captured.
- **Features: JSONL sidecar.** One `FeatureSnapshot` per line, keyed by
  `hop_index`.
- **Manifest: JSON.** Pins everything replay needs to rebuild an identical
  engine (see below). Without it, replay silently builds a *different* engine
  and the diff lies.

### Manifest contents (mandatory)

| Field | Why |
|---|---|
| `sample_rate` | engine config; mismatch changes hop timing |
| `block_size` (512), `channels` (1) | block contract |
| `world_hash` + path (`coach.json`) | the node graph must match |
| `app_version` / git SHA | float results are exact only for the same binary |
| `total_samples` | defines the partial-final-block policy |
| session/scale config (optional) | reconstruct full session if needed |

## Phases

### Phase 1 — Recorder (in the data-plane worker)

Tap **inside the worker** (not a decorator on the port): the decorator would
record pre-downmix interleaved callback frames — useful later for
capture-adapter bugs, but *wrong* for YIN. The worker tap captures the exact
mono 512-block stream the engine receives.

- After the block is popped (`data_plane.rs:318`) and before/at `process_block`
  (`:344`): push the block to a WAV writer.
- After the `FeatureSnapshot` read (`:357`): push the snapshot to the JSONL
  writer.
- **Heisen-recording guard:** disk writes go through a **bounded channel to a
  writer thread**. If it backs up, **mark the recording invalid** rather than
  stall the worker — stalling would *cause* the ring drops it is recording.
- WAV length is a multiple of 512 by construction (only whole consumed blocks
  written). Partial final block: not recorded; `total_samples` in the manifest
  documents it.

### Phase 2 — Shared engine builder (the one mandatory refactor)

Factor the engine construction (`build_pitch_engine` in app-coach's
`pitch_world.rs`) so **both the app and the headless harness build the engine
identically**. This is the linchpin — if they drift, the diff compares two
different engines. Reconcile with `dsp_bench`'s world-loading path so there is
one way to build the coach engine from `coach.json`.

### Phase 3 — Headless diff harness

- Reuse `dsp_bench`'s block loop: read `*-engine-input.wav`, push 512-blocks
  through a freshly-built engine (Phase 2), emit a new `*-features.jsonl`.
- **No ring, no threads, no Bevy** — the engine is pure, so run it
  synchronously.
- Diff two feature sidecars **on `hop_index`** (ignore `t_ms`; it is wall-clock
  and differs every run). Report octave-jump counts before/after.

### Phase 4 — Visual replay (`coach-game --replay-audio`)

- New adapter `domain-adapters/audio-wav/` implementing the **`AudioCapture`**
  port, reading `*-engine-input.wav` and feeding the ring **real-time paced**
  (sleep per buffer) so the UI animates naturally.
  - Its contract fits file playback (callback-based, adapter picks its thread,
    RAII session). One friction point: `StreamHandle` comes from device
    enumeration, so the adapter must also vend a fake `AudioDevices`.
  - Real-time paced → ring won't overflow. This path is **visual only**, not
    bit-exact (ring + timing re-enter); the bit-exact contract belongs to the
    headless path.
- Wire `coach-game -- --replay-audio <wav>` to install this adapter instead of
  cpal. Flag parsed in `main.rs`, mirroring `--replay`.

## Top risks (and mitigations)

| Risk | Mitigation |
|---|---|
| Fast replay overflows the ring → silent drops | UI adapter paces real-time; headless bypasses the ring entirely |
| Partial final block differs live vs replay | Don't record it; pin `total_samples` in manifest |
| "raw mic" vs "engine input" confusion | Name the file `*-engine-input.wav`; document it is post-downmix/post-ring |
| Engine-construction drift app vs headless | Phase 2 shared builder — the one mandatory refactor |
| Heisen-recording (recorder causes drops) | Writer thread + invalidate-on-backpressure, never stall the worker |
| Diff too strict | Compare on `hop_index`; normalize/ignore `t_ms` |
| Cross-machine float drift | Same binary = exact; cross-arch needs a tolerance (out of scope for now) |

## Explicitly deferred

- Pre-downmix / per-channel capture (a capture-adapter-bug tool, not a YIN
  tool).
- Cross-architecture float-tolerance diffing.
- Scrubbing / partial replay / speed control for the audio path.
- Any `traces/` retention policy (manual cleanup, as today).
- **The YIN fix itself** — this plan is the infrastructure to validate it. The
  octave-continuity / threshold / confidence-gating work is a separate plan.

## Provenance

Design reviewed in parallel by the software-architect agent and Codex
(`gpt-5.5`). Both independently rejected the single-adapter approach and
converged on the two-seam shape, post-downmix engine-mouth recording, float32
WAV, the shared engine builder, and the manifest. Codex surfaced the
`dsp_bench` reuse; the architect surfaced the Heisen-recording guard. Delete
this file once the work lands (per the repo convention — see commit `90aa33b`).
