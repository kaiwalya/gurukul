# SPEC: AppCoach

Decision log for the `AppCoach` port + adapter restructure. One page,
named decisions with one-line rationale. Not a design doc — the design
context lives in the conversation that produced this; this is the
record of what we chose to do.

Status: **v3 — scoped to v1 (control-plane only, like-for-like CLI
port). Pitch detection and the data plane are Phase 2; see §13.**

---

## 0. Scope

**v1 scope: move the three existing CLI behaviours behind a new
`AppCoach` boundary, with no new functionality.** The CLI today does:

1. `list-devices` — enumerate input devices.
2. `capture` — open a device, log per-frame stats for N ms.
3. `run` — boot/log/shutdown trivial main.

v1 ports those three behaviours to the new boundary. No pitch
detection, no DSP, no Mac-app wiring. The Mac app keeps doing its own
thing and is intended to be replaced once CLI functionality reaches
parity in a later phase.

**Phase 2 scope (deferred, not specified here):** `PitchDetector` port,
pitch worker thread, `ArcSwap<PitchReading>` snapshot, `latest_pitch()`
on the trait, mid-stream pitch toggling, the full data-plane half of
the two-plane architecture. The §7 architecture diagram and §13 note
keep Phase 2 in view so v1 decisions don't paint us into a corner, but
v1 ships without any of it.

Out of scope entirely: FFI binding shape, Mac-app migration, network
heads.

---

## 1. The two-system boundary

`AppCoach` is a sealed, self-managing subsystem behind a flat
command/event door. The head:

- constructs `AppCoach` once via `new(AppCoachDeps)`, passing port
  adapters (clock, telemetry, audio devices, audio capture);
- thereafter communicates **only** through the boundary defined in §2.

The head never touches `AppCoach`'s threads, internal channels, or
state. The head also never re-enters the adapters it passed in (it
hands them over).

This stipulation is the load-bearing constraint for everything below.

---

## 2. The head ↔ AppCoach boundary

Flat, FFI-survivable, three operations:

```rust
trait AppCoach: Send + Sync {
    fn send_command(&self, cmd: Command);
    fn poll_events(&self, out: &mut Vec<CoachEvent>);
    fn shutdown(&self, timeout: Duration) -> ShutdownResult;
}
```

- `send_command` is non-blocking. Enqueues onto the control plane.
- `poll_events` drains the outbound queue into the caller's buffer.
  Non-blocking; returns immediately with whatever's there.
- `shutdown` is explicit and blocks up to `timeout`. See §9.

**Outbound queue is bounded.** Capacity TBD at impl time (reasonable
default: 1024). On overflow the control plane drops the oldest
event(s) and emits a single `CoachEvent::EventsDropped { count }` when
space frees up. Heads SHOULD drain at ≥10Hz to avoid this.

**FFI translation note (Phase 2 / future concern).** Should a future
native head ever need a C ABI, the Rust `&mut Vec<CoachEvent>` becomes a
caller-allocated buffer + count return at the boundary
(`coach_poll_events(handle, *out_buf, cap) -> count`). The Rust trait
stays idiomatic; a future FFI binding would own the serialization seam.
Today's heads link the engine directly as a Rust crate — there is no
C ABI, and v1 is Rust-only.

**Phase 2 adds `latest_pitch()`.** The pitch snapshot read is a
fourth trait method that lands when the data plane lands. Designing
the v1 trait as if `latest_pitch` already existed (i.e. not folding
pitch into `poll_events`) keeps the Phase 2 addition non-breaking.

Rationale: the opaque-handle + poll shape survives a future C ABI
unchanged, so the eventual (if ever) FFI binding is a translation, not
a redesign.

**This is a breaking change to today's `trait AppCoach { fn main(...) }`.**
The one-shot `main` is replaced. Existing call sites in `coach-cli`
will be rewritten as part of this work.

---

## 3. The `Command` enum (intents from the head)

v1 set:

```rust
enum Command {
    ListDevices,
    StartSession(AudioConfig),
    StopSession,
    ConfigureSession { tuning: TuningSpec, tonality: Tonality },
}
```

- `ListDevices`: triggers a `CoachEvent::DevicesListed` reply. Async
  via the event queue, not a sync method, so all writes go through one
  surface.
- `StartSession`: kicks off `idle → starting → running` (or
  `idle → starting → error`). In v1 this just opens the AudioCapture
  stream; in Phase 2 it also spawns the pitch worker.
- `StopSession`: kicks off `running → stopping → idle`.
- `ConfigureSession`: sets the *musical* frame of reference (how the
  instrument is tuned + the scale the singer is in). **Decoupled from
  the audio lifecycle** — valid in any state, causes **no**
  `SessionState` change. The coach builds a `Tuning` from the spec,
  holds it with the `Tonality`, and on every configure publishes the
  **event-sourcing pair** (§4a): the `music_info()` snapshot (written
  first) and a `SessionConfigured` event (the log entry). Pitch
  *scoring* against the scale is a later phase; the seam — and now a
  recoverable record of the frame — exists so scoring (and any head)
  can plug in without re-plumbing the boundary. Bad payloads are a
  `debug_assert` today (only code builds them); this graduates to a
  runtime reject — keep the prior model — when an untrusted picker can
  reach it. The musical types (`InstrumentKey`, `TuningKind`,
  `Tonality`, `TuningSpec`, `MusicInfo`) live in `domain-ports::music`
  / `domain-ports::app_coach`; see `docs/MUSIC_MODEL.md`.

**Deferred to Phase 2:** `SetPitchEnabled` (no pitch yet).

**Deferred indefinitely** (do not add until a use case appears):
- `SwitchDevice` mid-session — needs a defined semantics
  (gap? crossfade?).
- `SetSampleRate` mid-session — same reason.

---

## 4. The `CoachEvent` enum (notifications to the head)

v1 set:

```rust
enum CoachEvent {
    DevicesListed { devices: Vec<InputDevice> },
    SessionStateChanged { new_state: SessionState },
    SessionConfigured { tuning: TuningSpec, tonality: Tonality },
    SessionError { kind: SessionErrorKind, reason: String },
    DefaultInputChanged { new_default: Option<DeviceId> },
    EventsDropped { count: u32 },
}

enum SessionErrorKind {
    DeviceUnavailable,   // device gone / refused open
    UnsupportedConfig,   // rate / channels / buffer rejected
    MidStreamFailure,    // cpal callback error mid-session
    Other,
}
```

- `SessionStateChanged` is the primary "something happened" event.
  Heads render off the `SessionState` enum.
- `SessionError` accompanies the `idle → error` transition (or
  `running → error` mid-stream). The `kind` lets heads branch /
  localize; the `reason` is free-form detail for logs.
- `DefaultInputChanged` is unsolicited (no command triggered it).
  Hotplug / OS default switch. Payload is the new default id only;
  heads re-issue `ListDevices` if they need the full list. In v1 the
  emitter for this is a TODO — there is no device-listener port yet,
  and we will not block v1 on adding one. The variant exists on the
  enum so heads can match on it exhaustively; v1 just never emits it.
- `SessionConfigured` is emitted on *every* `ConfigureSession` (not just
  the first), carrying the exact `tuning` + `tonality` that were applied.
  It is the **log entry** half of the event-sourcing pair (§4a) —
  decoupled from the audio lifecycle, so it fires whether or not a
  session is running.
- `EventsDropped` surfaces backpressure (see §2). One event per
  drop-burst, not one per dropped event.

**`RingOverrun` is Phase 2** (no data plane in v1, so no ring, so no
overrun to surface).

### 4a. The musical event-sourcing pair

`ConfigureSession` is the one command that mutates *musical* state, and
it does so through a snapshot+event pair, mirroring the
`audio_info` / `SessionStateChanged` pattern:

- **Snapshot — `music_info() -> Option<MusicInfo>`.** A lock-free
  read-cache (`ArcSwap`) holding the current `MusicInfo { tuning:
  TuningSpec, tonality: Tonality }`. Written **first**, before the event,
  so a head reacting to the event always reads a coherent snapshot.
  **Sticky**: `None` only before the first `ConfigureSession`; it
  survives session start/stop (unlike `audio_info`, which clears on stop)
  and is cleared only on shutdown. This is the "current musical frame"
  any head can poll at any time without replaying events.
- **Event — `SessionConfigured { tuning, tonality }`.** The append-only
  log entry. Folding the event stream reconstructs exactly what
  `music_info()` returns — the snapshot is a materialized view of the
  log. Emitted on every configure.

Why both: the snapshot is for *state* (what is the frame right now),
the event is for *reactions* (the frame just changed — repaint). A head
that only polls uses the snapshot; a head that reacts uses the event and
trusts the snapshot is already coherent when it does.

**`SessionErrorKind::WorkerPanic` is Phase 2** (no pitch worker in v1).

**Phase 2 adds pitch reads via `latest_pitch()`, not events.** Pitch
readings never enter the event queue.

---

## 5. `AudioConfig` and device selection

```rust
pub struct DeviceId(pub String);   // newtype — heads can't fabricate

struct AudioConfig {
    device_id: Option<DeviceId>,   // None = system default
    sample_rate: Option<u32>,      // None = AppCoach picks (prefer 48k)
    buffer_frames: Option<u32>,    // None = adapter default
}
```

`AudioConfig` carries the *audio* parameters only. The *musical*
configuration of a session (tuning + tonality — the frame of reference
for judging pitch) is carried separately by `Command::ConfigureSession`
and is decoupled from the audio lifecycle (see §3).

- Device picked by `persistent_id` string, not by handle. The head got
  the id from a prior `DevicesListed` event; the string survives FFI
  and serialization. The handle does not.
- `DeviceId` is a newtype so heads must pass through values they got
  from `DevicesListed` / `DefaultInputChanged`. They cannot invent ids.
- `sample_rate` / `buffer_frames` default to AppCoach's chosen policy
  (today: 48000 if supported; `sample_rate / 100` for buffer).
- **Stale `device_id`** (device vanished between `ListDevices` and
  `StartSession`) → session transitions to `Error` with
  `SessionErrorKind::DeviceUnavailable`. See §8.

---

## 6. The `SessionState` enum

```rust
enum SessionState {
    Idle,
    Starting,
    Running,
    Stopping,
    Error,
}
```

Transitions:

```
Idle ──StartSession──► Starting ──CaptureStarted──► Running
                              \──CaptureFailed──► Error
Running ──StopSession──► Stopping ──CaptureStopped──► Idle
Running ──CaptureFailed(mid-stream)──► Error
Starting ──StopSession──► Stopping (cancel in-flight Start;
                                    data plane aborts the open)
Error ──StopSession (idempotent)──► Idle
```

- Commands in unexpected states (e.g. `StartSession` while `Running`,
  `StartSession` while `Stopping`) are no-ops — silently dropped at
  the boundary, logged at `Debug` level via telemetry. Not an error:
  makes heads simpler and idempotent.
- `Stopping` and `Starting` exist because both involve a round-trip to
  the data plane that takes real time on macOS (~ms). Heads can show
  spinners.
- **Stop-during-Start (decision)**: cancel the in-flight start. The
  control plane forwards `ControlToData::AbortStartCapture` (or treats
  a `StopCapture` arriving before `CaptureStarted` as the abort
  signal — impl choice). The data plane tears down whatever was
  partially set up and acks `CaptureStopped`. No spurious `Running`
  flash. Rationale: snappier feel; user clicked Stop, they get Stop.
- Hotplug while `Running` on a device that vanishes: data plane emits
  `CaptureFailed { MidStreamFailure }`; control plane transitions
  `Running → Error`. See §8.

---

## 7. Two-plane architecture (internal)

The architecture is fully two-plane *as a design*. **v1 implements
only the control-plane half plus the AudioCapture stream;** the
pitch worker, the ring buffer, and the `ArcSwap` snapshot all arrive
in Phase 2. Designing the control plane against the eventual two-plane
shape means Phase 2 is additive, not a rewrite.

**The control plane is single-threaded.** One thread, one MPSC drain
loop, and **all session-state mutation happens here**. This is the
load-bearing invariant: no `Mutex` around session state, no race, no
ordering question. Inputs:

- inbound commands from the head (via `send_command`);
- `DataToControl` acks from the data plane;
- device-change notifications from a future device-listener port (out
  of scope for v1 — modelled as TODO).

Data plane in v1 is just the AudioCapture stream:

- audio RT thread (cpal callback): receives `f32` frames. In v1 the
  callback logs per-frame stats at `[DEBUG]` (parity with today's
  `capture` subcommand). In Phase 2 it writes a lock-free ring instead.
- pitch worker thread: **Phase 2 only.** Does not exist in v1.

Internal message types in v1:

- `ControlToData`: `StartCapture { device, cfg }`, `StopCapture`,
  `Quit`.
- `DataToControl`: `CaptureStarted`, `CaptureStopped`,
  `CaptureFailed { kind, reason }`, `WorkerExited`.

In v1, "the data plane" is just the open `CaptureSession` (held by the
control-plane thread, since `CaptureSession` is `!Send` on macOS). The
acks come from inline error handling around the cpal `build_input_stream`
+ `stream.play()` calls, not from a separate worker thread.

Phase 2 adds:
- `ControlToData::SetPitchEnabled(bool)`
- `DataToControl::RingOverrun { dropped_frames }`
- a real pitch worker thread that owns the ring drain and the
  `ArcSwap<PitchReading>` snapshot.

`ControlToData::Quit` and `DataToControl::WorkerExited` are the
shutdown handshake (Phase 2). In v1, shutdown just drops the
`CaptureSession` synchronously on the control-plane thread.

---

## 8. Failure modes — the `Error` state

Cases that transition to `Error` (with
`SessionError { kind, reason }` event):

- `StartSession` and `AudioCapture::open` returns
  `UnsupportedConfig` / `DeviceUnavailable` / `InvalidHandle` →
  `kind = DeviceUnavailable` or `UnsupportedConfig`.
- `StartSession` with a stale `device_id` (vanished since last
  `ListDevices`) → `kind = DeviceUnavailable`.
- Mid-session cpal stream error delivered via the error callback (e.g.
  device unplugged). Currently logged-and-dropped in
  `adapter-audio-cpal` — this work routes it as
  `DataToControl::CaptureFailed { MidStreamFailure }`.

**Phase 2 adds:** worker-panic detection
(`SessionErrorKind::WorkerPanic` via control-plane watchdog on
`DataToControl` channel disconnect). v1 has no worker to panic.

Recovery:
- `StopSession` from `Error` returns to `Idle` (cleanup + reset).
- No automatic retry. Heads decide whether to re-issue `StartSession`.

---

## 9. AppCoach lifecycle

- Construction (`new(deps)`): allocates the command channel and the
  outbound event queue; **eagerly spawns the control-plane thread**.
  The data plane in v1 is the `CaptureSession` opened on `StartSession`
  and dropped on `StopSession` / shutdown — same thread, no spawn.
- Shutdown: **explicit `shutdown(timeout)` + `Drop` as belt-and-
  suspenders.**

```rust
enum ShutdownResult {
    Clean,
    TimedOut,           // forced; resources may have leaked
    AlreadyShutDown,
}

fn shutdown(&self, timeout: Duration) -> ShutdownResult;
```

  `shutdown` sends an internal Quit and waits up to `timeout` for the
  control-plane thread to join (which includes dropping any open
  `CaptureSession`). On timeout it detaches the thread (logs the leak
  via telemetry) and returns `TimedOut`.

  `Drop` calls `shutdown(Duration::from_millis(0))` if shutdown wasn't
  called explicitly — last-resort cleanup, logs a warning that the
  head should have called `shutdown()`.

  **Default timeout: 5 seconds.** Long enough that we essentially
  never force-drop in normal use, even when cpal teardown sticks
  (a known macOS issue). Heads pass their own value if they want
  snappier exit semantics.

`AppCoach` is **not `Send`** at the boundary (CaptureSession isn't
either — see audio_capture.rs docs). Heads construct, use, shut down,
drop on the same thread that constructed it. Matches FFI ergonomics
when those land.

---

## 10. Recorder / debug tap

**v1: no recorder, no subscription registry.** Telemetry hooks inside
the control-plane drain loop are hardcoded (`event` + outbound queue
push are two function calls; that's the whole fan-out).

Re-evaluate when a recorder is genuinely wanted as a runtime-toggled
thing. At that point, add a minimal subscription mechanism to the
control plane only.

---

## 11. Dependencies on other ports (v1)

`AppCoachDeps` after this work:

```rust
struct AppCoachDeps {
    pub clock: Arc<dyn Clock>,
    pub telemetry: Arc<dyn Telemetry>,
    pub audio_devices: Arc<dyn AudioDevices>,
    pub audio_capture: Arc<dyn AudioCapture>,
    pub host_version: &'static str,
}
```

All four ports already exist. v1 has no port-creation work — just the
trait restructure + the adapter implementation + the CLI rewrite.

---

## 12. Implementation PRs (v1)

Three PRs, sequenced. Each leaves the workspace green.

1. **PR 16 — `DeviceId` newtype.** Add `pub struct DeviceId(pub String)`
   to `domain-ports::audio_devices`. Update `InputDevice.persistent_id`
   and call sites. Mechanical.
2. **PR 17 — `AppCoach` trait restructure.** Replace `fn main(deps)`
   with the v1 boundary in `domain-ports::app_coach`. Add all the v1
   enums. `adapter-app-coach` body becomes `todo!()`. **Workspace will
   not link CLI until PR 18 lands** — these two should merge together
   or back-to-back.
3. **PR 18 — `adapter-app-coach` v2.** Control-plane thread + drain
   loop + state machine. `StartSession` opens AudioCapture and the
   per-frame `[DEBUG]` log lives in the callback (parity with today's
   `capture` subcommand). Bounded outbound queue + `EventsDropped`.
   `shutdown(timeout)` with 5s default. Stop-during-Start = cancel
   in-flight. Tests via existing `TestTelemetry`/`TestClock` fakes.
4. **PR 19 — `coach-cli` rewrite.** Three subcommands rewired as "send
   command, poll until terminal event, print." Old `WindowAggregator`
   path is gone (per-frame log lives in the adapter now).

After PR 19 the CLI's externally observable behaviour matches today's,
just routed through `AppCoach`.

---

## 13. Phase 2 — what's deferred and why

These are out of v1 scope but the v1 design is shaped to receive them
without rework. Listed for clarity, not as a roadmap commitment.

- **`PitchDetector` port.** Pull-shape (AppCoach owns the worker
  thread; the port adapter is stateless w.r.t. threading). Wraps
  `node-pitch-yin`.
- **Pitch worker thread** in the data plane. Drains the audio ring,
  runs YIN, writes `ArcSwap<PitchReading>`.
- **`latest_pitch() -> Option<PitchReading>`** on the trait — fourth
  method, non-breaking addition over v1.
- **`PitchReading`** with a `session_gen` tag (monotonic, bumped on
  each `StartSession`) so heads can detect stale reads.
- **`Command::SetPitchEnabled(bool)`** — pitch worker stays hot,
  suppresses output on disable, publishes `None` once on disable
  transition so heads clear their display.
- **`SessionErrorKind::WorkerPanic`** + control-plane watchdog on
  channel disconnect.
- **`DataToControl::RingOverrun`** — internal-only (logs to
  telemetry); surface as `SessionDegraded` only if sustained overruns
  become a product concern.

---

## Resolved questions (from architect review #2 + user)

1. `SetPitchEnabled`: stays hot, suppresses output, publishes `None`
   on disable. Phase 2.
2. Silent no-ops vs `CommandIgnored` event: silent + debug telemetry.
   No event. §6.
3. Shutdown shape: explicit `shutdown(timeout)` + `Drop` as fallback,
   5s default timeout. §9.
4. `PitchDetector` push vs pull: pull. Phase 2 / §13.
5. Stop-during-Start: cancel in-flight start. §6.
6. v1 scope vs full spec: v1 is control-plane only + AudioCapture;
   pitch and data plane are Phase 2. §0 / §7 / §13.
