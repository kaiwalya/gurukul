# Spec: Phase 1.6.1 — Mic on iOS (capture lifecycle seam)

Get the microphone working on iOS, and in doing so close a latent gap
that exists on **every** platform: the capture path has no way to react
when the OS takes the mic away mid-session. This spec is the *how*;
the [`ROADMAP.md`](ROADMAP.md) phase line and
[`PHASE_1_6_PLAN.md`](PHASE_1_6_PLAN.md) §1.6.1 are the scope of record.

Grounded in two design reviews (architect + Codex, **twice** — high-level
then this detailed spec) and a prior-art study of Apple
AVAudioSession/AVCaptureSession, Android Camera2, and INDI/PipeWire/
CoreMIDI. Where a decision cites prior art, the citation is load-bearing.

## The problem (three gaps, one root)

The iOS spike found three gaps. They share a root: **our capture port is
fire-and-forget with a single exit.** `open()` starts a stream; dropping
the returned session stops it. The only adapter→app channel is the
per-frame callback. Nothing flows back out about *what the device
actually gave us* or *what the OS later did to us*.

| # | Gap | Why it exists |
| --- | --- | --- |
| 1 | iOS mic never opens | cpal 0.17's iOS backend touches `AVAudioSession` only to read/set buffer size. It never sets category `.record`, never activates the session, never requests record permission. We must do that ourselves, before cpal touches the AudioUnit. |
| 2 | Engine built for the wrong format | iOS imposes its own sample rate / IO buffer. `open()` is handed the *requested* rate and silently runs the engine at a guess. |
| 3 | No reaction to mid-session loss | iOS interruptions (calls/Siri), route changes (headphones), permission denial all happen *after* open. There is no seam for the adapter to tell the app "your stream died, here's why." cpal's error closure just `eprintln!`s into the void (`capture.rs:73`). |

Gap 3 is **not iOS-specific**: a USB-mic unplug or AirPods route change on
Mac is the same architectural event. Today Mac handles none of it (silent
death). So the fix is a **platform-neutral port seam, fed per-platform** —
not an iOS bolt-on.

## Build order (revised after review)

Both reviews flagged that the original "seam first" order gold-plates a
seam we can't yet validate against the real thing. First sound needs gaps
2 and 1; gap 3 is robustness on top.

| Step | Gap | Closed-loop testable on Mac? |
| --- | --- | --- |
| **1.6.1a** | 2 — negotiated config + fail-closed open | ✅ Yes — and fixes the latent Mac guess-bug |
| **1.6.1b** | 1 — iOS session + permission | Partial (simulator; real prompt is 1.6.2) |
| **1.6.1c** | 3 — lifecycle seam + head observation | ✅ Yes — driven by a fake/injectable event source |

Mac is the test bed by **testability**, not by API: it runs closed-loop
without a device-and-human. We prove the risky platform-neutral halves
(2 and 3) on Mac with deterministic synthetic events, then bolt the small
iOS-specific feed on after.

---

# Decision 1 — config: negotiate first, `open()` fails-closed

## The contract (this is the load-bearing decision)

**`open()` is handed an exact config. It either delivers that config, or
it returns `UnsupportedConfig { wanted, actual }` and starts no stream.**
It never silently succeeds at a different format and asks the caller to
inspect frames afterward. (Settled by the user; see
[[feedback_fail_closed_ports]].)

This kills the race both reviewers flagged: with "open then verify then
rebuild," frames can flow into a wrong-format engine before the caller
notices (Codex #2, architect "rebuild ordering"). With fail-closed, the
mismatch path **starts no stream** — there is nothing to rebuild and no
stray frame.

For `open()` to be handed an *exact* config, the exact config must be
learned **before** the engine is built. That is what `negotiate()` is for.

## The flow

iOS cpal `supported_input_configs()` reads the **live** `AVAudioSession`
and returns a **single exact config** (min == max) — but only **after
`setActive(true)`** (Apple QA1631: pre-activation values are stale hints).
Pre-activation it returns nothing, surfacing as `SampleRateSupport::
ProbeOnly` (`devices.rs:67`). So session activation must precede *both*
enumeration and negotiate (Codex #1, architect "enumeration dependency").

```
0. [iOS] prepare + activate AVAudioSession  ── makes the live session readable
1. enumerate (list_devices/default_input)   ── now returns the exact iOS config
2. negotiate(handle, requested) → CaptureInfo ── the EXACT format open() will get
3. build engine for CaptureInfo.sample_rate ── engine is right the FIRST time
4. open(handle, exact_cfg, on_frame, sink)  ── delivers it, or UnsupportedConfig (no stream)
5. publish AudioInfo from CaptureInfo        ── negotiated values, fixes today's bug
6. → Running
```

Why `negotiate()` is a **distinct method** (settled, vs folding into
enumeration): the data plane is built *before* `open()` today — the cpal
frame callback needs its ring producer in hand (`control_plane.rs` builds
`DataPlane` then opens capture). So the rate must be known before open. A
dedicated preflight `negotiate()` keeps "learn the real format" a first-
class step rather than overloading enumeration's meaning, and gives a
clean home for the iOS-specific post-activation read.

## Port changes

```rust
// audio_capture.rs
pub struct CaptureInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: Option<u32>,   // None when the backend can't know it
                                      // pre-callback (cpal Default); see note.
}

trait AudioCapture {
    // NEW: preflight — learn the EXACT format before the engine is built.
    // On a ProbeOnly stream, negotiate may dry-open to discover it; on iOS
    // it reads the active session. Returns UnsupportedConfig if the
    // requested config can't be met.
    fn negotiate(&self, handle: &StreamHandle, requested: &CaptureConfig)
        -> Result<CaptureInfo, CaptureError>;

    // CHANGED: takes the lifecycle sink; FAILS CLOSED on format mismatch.
    fn open(&self, handle: StreamHandle, cfg: CaptureConfig,
            on_frame: CaptureCallback, on_event: LifecycleSink)
        -> Result<CaptureSession, CaptureError>;
}
```

`buffer_frames` is `Option<u32>` (Codex #9): cpal with `BufferSize::
Default` can't report actual buffer size before callbacks fire; iOS knows
it post-activation. `None` means "not known pre-stream," not zero.

`open()` no longer returns `CaptureInfo` — `negotiate()` already produced
it, and fail-closed means open's success *is* the confirmation. One value,
one source.

### Three contract details to nail before 1.6.1a (both reviewers)

1. **`AudioInfo.buffer_frames` must also become `Option<u32>`** (architect,
   "the one thing to nail"). Today it's a non-optional `u32` and the start
   path papers over absence with `unwrap_or(0)` (`control_plane.rs:279`).
   If `CaptureInfo`'s `Option` lands there as `0`, we reintroduce the exact
   "zero vs unknown" lie the `Option` kills — and the negotiation test
   can't assert the buffer. Propagate the `Option` to `AudioInfo`; drop the
   `unwrap_or(0)`. (Small public-surface break — it's on the change list.)

2. **`CaptureError::UnsupportedConfig` must carry `{ wanted, actual }`**
   (Codex #4). Today it's `{ reason: String }` only. Fail-closed diagnosis
   and the 1.6.1a test ("assert the mismatch is reported, no stream
   starts") need the structured pair, not a string.

3. **`negotiate()` on a `ProbeOnly` stream opens the device twice** (both):
   there is no "probe without play" path in cpal — `build_input_stream` +
   `play()` is the only motion (`capture.rs`). So a ProbeOnly `negotiate()`
   briefly runs a live stream with a throwaway callback, drops it, then
   `open()` runs it again; the first frames are discarded. Acceptable, but
   stated so it isn't a surprise. On iOS this path is **not** taken —
   post-activation enumeration yields the exact config without a dry-open.
   On ProbeOnly, `buffer_frames` stays `None` (a dry-open validates
   rate/channels, not the eventual buffer).

---

# Decision 2 — iOS session + permission (gap 1)

iOS-only glue (`#[cfg(target_os = "ios")]`), behind the capture adapter.
cpal does **not** own the session — we own an `AVAudioSession` alongside
it via `objc2-avf-audio` (already in the tree; cpal pulls it). Order, per
Apple's docs:

1. `setCategory(.record` or `.playAndRecord)` — preferred values only.
2. `requestRecordPermission` — **async**; gate activation on `granted`.
3. `setActive(true)` — negotiates with hardware.
4. *Only now* enumerate / `negotiate()` (the live session is readable).

This runs **before** enumeration and open — enumeration on iOS returns
`ProbeOnly` without an active record-capable session.

## Async permission needs a real pending-start state

This was underspecified in the first draft; both reviewers flagged it
(Codex #3, architect). `requestRecordPermission` is async and, first-
launch, blocks on a user dialog. The control thread is a `recv_timeout`
mailbox drain — it must **not** block inside `open()` waiting for the
dialog. So:

- `StartSession` accepted → if permission undetermined, fire the async
  request, store the **pending start intent** (the requested `AudioConfig`
  + device policy, see Decision 3) + the **current generation token**, and
  return to the mailbox in state `Starting`.
- The permission callback (arbitrary thread) enqueues a new mailbox input
  `PermissionResolved { granted, generation }`. The control thread, back
  in its loop, continues the start sequence (enumerate → negotiate → build
  → open) **only if** the generation still matches.
- **`StopSession` while a dialog is open** → cancel the pending start;
  a late `PermissionResolved` with a stale generation is ignored. (No
  race into an unwanted start — Codex #3, architect.)
- **Denied is terminal**: later requests return `false` with no dialog;
  only Settings re-enables. → `Error(PermissionDenied)`, never auto-retry.

---

# Decision 3 — the lifecycle seam (gap 3): events trigger, state decides

## Shape: a Send sink into the existing control-plane mailbox

The adapter is handed a **`Send` sink** (`LifecycleSink`, a cloned
`Sender<Input>` wrapper) and pushes a `LifecycleEvent` when the OS (or a
fake source) signals. The control plane already drains one mailbox; the
event arrives as a new `Input` variant. One select point, one place state
mutates. (Both reviewers endorsed second-callback-in-`open()` over a
returned receiver; routing into the existing mailbox is what both wanted.)

Hard constraint from `!Send`: AVAudioSession observers fire on arbitrary
threads — notably `routeChangeNotification` on a *secondary* thread, while
interruption & mediaServicesReset post on **main** (Apple; non-uniform).
The sink is `Send` and **only enqueues** — it never drops the session or
mutates state on the notification thread. The `!Send` `CaptureSession` is
dropped on the control-plane thread, always. The sink hangs off a `Send`
object, **not** off `CaptureSession`, and must not keep the channel alive
past shutdown.

## Generation token: reject stale events (both reviewers)

A monotonic generation counter, owned by the control thread, gates every
async input. The rule (both reviewers' top finding on the second pass —
incrementing on *start* alone is not enough):

| Action | Effect on generation |
| --- | --- |
| Start accepted | **increment**; snapshot the new value into the sink closure handed to `open()`, and into any pending-permission state |
| Stop accepted | **invalidate** (bump) — and clear pending/resume intent |
| `LifecycleEvent` received | process only if its generation **matches current** *and* the state/private intent allows it; else drop |
| `PermissionResolved` received | same check; stale or canceled intent is ignored |

The counter lives in `ControlPlane` (control-thread-only, not a shared
atomic). `open()`-time reads it and **bakes the value into the sink
closure** — the sink is a plain `Send` enqueuer carrying a fixed
generation, not a reader of live state. A callback from an old, dropped
session (route/error notifications arrive late) carries a stale generation
and is dropped. Because **stop also bumps**, a late event after a user
quit can't match either — closing the hole that "increment on start only"
left open.

## Recovery: state-reconciling, not event-replaying

The load-bearing decision, and the answer to "discrete events vs
observable state." INDI/PipeWire model **observable state** (reconcile
against current truth); Apple/Camera2/CoreMIDI model **discrete events**
(edge-triggered, low-latency). For a single-consumer real-time capture
path fed by Apple's own event APIs, events are the right *trigger* — "a
call arrived" is an edge, not a steady state.

But we borrow INDI's robustness: **an event does not encode its own
recovery — it triggers a re-evaluation that reads current truth.**
Recovery re-runs the start sequence (re-enumerate → renegotiate → reopen →
reset engine) reading live state + the **stored start intent**. A missed,
coalesced, or duplicated event is safe — it re-derives the same state.

Recovery reads the **stored start intent** (Codex #8, architect): the
control plane must retain the requested `AudioConfig` + device policy
(specific persistent-id vs "default input") after a successful start.
This also makes the verdict table context-sensitive (Codex #7): a route
change that loses the *specific* device is terminal, but if intent was
"default input," recovery re-selects the new default and keeps running.
**Events trigger reconciliation; reconciliation decides terminal vs
recoverable** — the table below is the *first* classification, refined by
what re-evaluation finds.

## Guards against spin and wedge (both reviewers)

The state-reconciling model is safe against duplicate events but **not**
against a recovery that keeps failing or an event that keeps re-firing.
Three guards (this is what INDI/PipeWire do that we borrowed philosophy
from but not the mechanism):

| Guard | Against | Rule |
| --- | --- | --- |
| **Retry budget** | reopen → fail → reopen spin | After K consecutive failed reopens → `Error`. Reset on a clean Running. |
| **Route-change debounce** | Apple emits route changes in bursts during settling | Coalesce route changes within ~N ms before one reconcile. |
| **Interruption timeout** | `InterruptionEnded` never arrives (Apple drops it in some backgrounding cases) | Bounded wait while Resume is locked, then leave it locked but allow a manual quit (never auto-resume). |

**This reshapes the control loop (architect).** All three guards are
time-based, but the loop today is a fixed `recv_timeout(500ms)` whose
timeout branch does nothing. Implementing them means: track pending
deadlines as control-plane state, compute a *per-iteration* timeout (next
deadline, not a constant), and do real work in the timeout branch. This is
a **structural change to `run()`**, not the additive "store a counter" the
mechanical list might imply. It lands in 1.6.1c, not 1.6.1a.

## Event taxonomy (from Apple + Camera2)

Apple's explicit warning: do **not** collapse "interruption ended"
(conditional resume on a hint) with "route changed" (unconditional
re-read). Different paths.

```rust
enum LifecycleEvent {                          // each carries a generation token
    Interrupted,                               // .began — pause, do NOT reopen
    InterruptionEnded { should_resume: bool }, // resume iff hint set
    RouteChanged,                              // re-read config, keep running
    MediaServicesReset,                        // tear down + rebuild all audio objects
    DeviceUnavailable,                         // device lost/disconnected
    PermissionDenied,                          // terminal; route user to Settings
    BackendError { reason: String },           // cpal stream error (today's eprintln dead-end)
}
```

| Event | Verdict (Phase 1) | Control-plane action |
| --- | --- | --- |
| `Interrupted` | recoverable, two-phase | stop capture, clear `AudioInfo`; drive head → **Pause screen with Resume *locked*** (see Decision 4); arm interruption timeout |
| `InterruptionEnded{should_resume}` | conditional | **unlock Resume** (player taps it to resume — never auto-resume). If `!should_resume`, leave locked; player may quit |
| `RouteChanged` | recoverable, silent | debounce → renegotiate; if intent satisfiable keep running, else terminal |
| `MediaServicesReset` | **terminal in 1.6.1** | → `Error`; (recoverable rebuild deferred to 1.6.2) |
| `DeviceUnavailable` | terminal *for that device* | if intent = "default", re-select; else → `Error(MidStreamFailure)` |
| `PermissionDenied` | terminal | → `Error(PermissionDenied)`; surface "enable in Settings" |
| `BackendError` | terminal | → `Error(MidStreamFailure)`; surface |

`classify_lifecycle_event` maps mid-stream `BackendError` /
`DeviceUnavailable` → **`SessionErrorKind::MidStreamFailure`** (architect;
the variant exists but nothing produces it today) — distinguishing a
mid-session death from a start-time failure for the head.

**Phase-1 scope on `MediaServicesReset`:** it's rare and its handler is
the riskiest path, and we can't trigger it on Mac. It is **terminal** in
1.6.1 (treated like `BackendError`); promote it to the recoverable
rebuild in 1.6.2 on-device where it's actually exercisable. Phase-1's
recovery proof is `Interrupted`/`InterruptionEnded`/`RouteChanged` +
terminal-everything-else.

## State-machine: the public contract (closed enum)

The coach's `SessionState` stays a closed enum (`Idle/Starting/Running/
Stopping/Error`) — **no new `Paused` variant on the coach side**. An
interruption stops capture and clears `AudioInfo` before leaving `Running`
(honoring the `AudioInfo iff Running` invariant, Codex #6 / architect).
The *distinction* between "user quit" and "OS interrupted" lives in the
**head**, not the coach state — see Decision 4. The coach simply stops; it
need not encode why.

| Situation | Coach `SessionState` |
| --- | --- |
| Interruption begin | `Running → Stopping → Idle` (capture stopped, `AudioInfo` cleared) — same transition as a user stop |
| Recoverable reopen in flight | head observes `Running → Stopping → Starting → Running` |
| Any terminal verdict | `Error(kind)`, `AudioInfo` cleared, data plane stopped |

A new `SessionErrorKind::PermissionDenied` is added (Codex #10) so the
head can show "enable in Settings" rather than a generic device error.

## Threading rule

All session mutation + recovery goes through the single control-plane
thread. Notifications are marshalled onto it via the `Send` sink — never
trust the posting thread, never mutate or drop on it.

---

# Decision 4 — the head observes coach state; interruption = Pause with Resume locked (1.6.1c, blocker)

Both reviewers caught that "the head tolerates an unsolicited stop" was
described as a one-liner but is **missing entirely**: the head's
`SessionStateChanged` handler is a single `info!` log line, and nothing
maps a coach `Stopping`/`Idle`/`Error` back to a screen change. Without
this, a recovery cycle is invisible (live game over a dead session) and a
terminal error wedges the user in `InGame` forever. The seam is
**untestable end-to-end** without it. So head observation is **in scope
for 1.6.1c**.

## The head already has a Pause screen — reuse it (architect)

The head has an existing `AppState::Paused` (`apps/coach-game/src/
state.rs`) with a live mechanic: entering it stops the coach session,
resuming starts a fresh one, a "Continue" affordance is shown. The second-
pass review found the prior draft designed the coach-side mapping **without
reconciling against this** — so "interruption-pause" and "user-quit" were
indistinguishable (both → coach `Idle`). The fix is to make the
distinction live where it belongs: the head's Pause screen.

## Interruption (phone call / Siri) = Pause screen, Resume locked (user-settled)

A phone call should feel like **the player hit Escape — the Pause menu
appears — but Resume is *disabled*** until the call ends. No silent
auto-resume: the player needs a beat to start singing (and a future
metronome count-in lives here).

| Moment | Head behavior |
| --- | --- |
| `Interrupted` (call begins) | enter `AppState::Paused` programmatically, **Resume action disabled** (mic is gone; resuming is impossible) |
| `InterruptionEnded{should_resume:true}` | **enable Resume**; player taps it to start a fresh session (the existing Paused→resume path) |
| `InterruptionEnded{should_resume:false}` | leave Resume disabled; player may quit to menu |
| Interruption timeout (ended never arrives) | Resume stays disabled; player may quit |

This **is** the pending-resume marker both reviewers said was missing — it
is a real, user-visible bit ("is Resume enabled?"), not a hidden flag, and
it makes the two cases structurally distinct:

| Cause | Screen | Resume |
| --- | --- | --- |
| User pressed Escape | Pause | **enabled** |
| OS interruption | Pause | **disabled** until `InterruptionEnded` |

## The rest of the head's reactions

| Coach state (unsolicited) | Head reaction |
| --- | --- |
| `Error(kind)` | leave `InGame` → menu; surface the reason (`PermissionDenied` → "enable in Settings", else generic) |
| recovery cycle (`Running→Stopping→Starting→Running` the head didn't command, e.g. `RouteChanged`) | tolerate silently; a "reconnecting" affordance is *polish*, deferred to 1.6.5 |

The user's framing governs the split: the seam **fails with a specific
error** the head can act on; the head reacts to that error (and to the
interruption signal), rather than inspecting low-level state. Polished
reconnect UX is 1.6.5; correct interruption-pause + exit-on-terminal-error
is 1.6.1.

---

# Testing — closed-loop on Mac with a fake event source

Steps 1.6.1a and 1.6.1c are provable headless on Mac:

- **Negotiation + fail-closed (a):** a fake capture adapter whose
  `negotiate()` returns a rate differing from the request; assert the
  engine is built for the negotiated rate and `AudioInfo` publishes
  negotiated values. A second fake whose `open()` returns
  `UnsupportedConfig`; assert **no stream starts** and the session goes to
  `Error`, not a wrong-format Running. Drives out the latent Mac guess-bug.
- **Lifecycle seam (c):** a fake event source pushes each
  `LifecycleEvent` (with generation) on command. Assert the control plane
  drives the right transition per the taxonomy table; assert a stale-
  generation event is dropped; assert a duplicate/coalesced event is
  idempotent (same end state); assert the retry budget trips to `Error`
  after K failures and the interruption timeout resolves the paused state.
  Fits the existing `FakeCapture` (`app-coach/src/lib.rs`) — the seam's
  injection point — and `FakeCoach` for head-side canned state events.

iOS-real validation (the privacy prompt, hardware route, true
interruption, `MediaServicesReset`) is **1.6.2** on device — the seam is
proven on Mac first.

---

# What does not change

Engine, analyzers, tuning/scale model, read-model resources, the
`AppCoach` port boundary, the `Idle/Starting/Running/Stopping/Error`
state names (no new coach `SessionState` variant; the head reuses its
existing `AppState::Paused`). This phase is **port
surface (negotiate + fail-closed open + lifecycle sink) + control-plane
recovery/guards/generation + iOS adapter feed + minimal head observation**
— no DSP, no domain logic.

# Mechanical change list (signature breaks + additions)

| Item | Action | Step |
| --- | --- | --- |
| `CaptureError::UnsupportedConfig` | change `{ reason }` → `{ wanted, actual }` (structured, fail-closed diagnosis) | 1.6.1a |
| `CaptureInfo` (new) + `AudioCapture::negotiate` (new) | the preflight that learns the exact format | 1.6.1a |
| `AudioInfo.buffer_frames` | `u32` → `Option<u32>`; drop the `unwrap_or(0)` (`control_plane.rs:279`) | 1.6.1a |
| `WavAudioCapture` (`audio-wav/src/capture.rs`) | add `on_event` arg to `open` (ignored — never-ending mic); add `negotiate()` returning the WAV header rate | 1.6.1a |
| `FakeCapture` (`app-coach/src/lib.rs`) | add `on_event` arg to `open` — the 1.6.1c event-injection hook; add `negotiate()` | 1.6.1a |
| `SessionErrorKind` | add `PermissionDenied` | 1.6.1c |
| `classify_lifecycle_event` | new fn beside `classify_open_error` (`helpers.rs`); map mid-stream `BackendError`/`DeviceUnavailable` → `MidStreamFailure` | 1.6.1c |
| control plane | store start intent; generation counter (bump on **start and stop**); `PermissionResolved` + `LifecycleEvent` mailbox inputs; **reshape `run()` to deadline-aware timeout** for retry/debounce/interruption guards | 1.6.1c |
| head (`coach.rs` + `state.rs`) | observe coach state; on terminal `Error` → exit `InGame`; on `Interrupted` → `AppState::Paused` with Resume **disabled**, on `InterruptionEnded` → enable Resume | 1.6.1c |

# Explicitly deferred

- **Mac CoreAudio feed** for the seam (real Mac route/unplug events). The
  seam works with the fake source; the CoreAudio-listener feed is a small
  follow-up, not a 1.6.1 blocker.
- **`MediaServicesReset` recoverable rebuild** → 1.6.2 (on-device only).
- **"Reconnecting" UX polish** → 1.6.5. 1.6.1 lands exit-on-terminal-error.
- **Android** (1.6.6).

# Provenance

Design reviewed twice by the software-architect agent and Codex (gpt-5.5)
— high-level approach, then this detailed spec. Both reviews converged on
the same revisions (head observation, generation token, async-permission
pending state, guards against spin/wedge, fail-closed open). Prior-art
idioms verified against Apple developer docs (QA1631,
AVAudioSession/AVCaptureSession), the Android Camera2 reference
(StreamConfigurationMap, `onDisconnected`, threading), and
INDI/PipeWire/CoreMIDI surveys.
