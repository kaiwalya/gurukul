# Plan: Phase 1.6.1b — iOS AVAudioSession + mic permission

Step "b" of Phase 1.6.1. The *why* and the cross-cutting decisions live
in [`PHASE_1_6_1_SPEC.md`](PHASE_1_6_1_SPEC.md) (Decision 2); this file is
the settled *how* for 1.6.1b, after a design session and two rounds of
architect + Codex review. 1.6.1a (negotiate + fail-closed open) is done.

This is the **least Mac-testable** step: the real privacy prompt, session
activation, and hardware route only exist on an iOS device/simulator —
full on-device validation is 1.6.2. We prove the platform-neutral state
machine on Mac with a fake.

## Ground truth (verified, not assumed)

- `objc2-avf-audio 0.3.2` is in `Cargo.lock` but only **transitively**
  (via cpal). To call `AVAudioSession` ourselves it must become a
  **direct, iOS-target-gated** dep of `adapter-audio-cpal`.
- iOS permission API (from the binding's own Apple doc text):
  - `recordPermission()` — **sync** read of current state
    (`Undetermined | Denied | Granted`). No dialog.
  - `requestRecordPermission(block)` — **non-blocking**; returns at once.
    The block fires **later, on an arbitrary thread**, and fires
    *immediately* if already decided.
  - `setActive(true)` — can **fail** even after permission is granted.
- Android is the same fire-and-callback shape.

**Consequence:** the OS never offers a blocking "wait for the answer"
call. The async is intrinsic; any design must be "fire now, receive
later." The control thread (a `recv_timeout` mailbox drain) must not
block on it.

---

## Part A — the `Audio*` / `Music*` rename (do this FIRST, separately)

The coach CQRS protocol mixes two domains. Tag them so a type like
`AudioSessionState` is self-documenting. Mechanical (the compiler catches
every missed call site); lands as its own commit before the feature.

| Layer | Current | New |
| --- | --- | --- |
| Command (audio) | `ListDevices` / `StartSession` / `StopSession` | `AudioListDevices` / `AudioStartSession` / `AudioStopSession` |
| Command (music) | `ListScales` / `ConfigureSession` | `MusicListScales` / `MusicConfigureSession` |
| Event (audio) | `DevicesListed` / `DefaultInputChanged` / `SessionStateChanged` / `SessionError` | `AudioDevicesListed` / `AudioDefaultInputChanged` / `AudioSessionStateChanged` / `AudioSessionError` |
| Event (music) | `ScalesListed` / `SessionConfigured` | `MusicScalesListed` / `MusicSessionConfigured` |
| Type | `SessionState` / `SessionErrorKind` | `AudioSessionState` / `AudioSessionErrorKind` |
| **Unchanged** | `AudioConfig`, `AudioInfo`, `MusicInfo` (already prefixed); `EventsDropped` (infra); `Scale`, `InputDevice`, `DeviceId` (domain nouns / other-port types) | — |

---

## Part B — the iOS session + permission feature

### B1. The provider-factory contract (new port)

The async permission lives in **how the `AudioDevices` provider is
constructed**, not in new methods on the `AudioDevices`/`AudioCapture`
traits (both stay unchanged). A provider that yields a live `AudioDevices`
is, by construction, one whose session is active — *unprepared is
unrepresentable*.

New trait (names final), injected into `AppCoachDeps` in place of the
ready `Arc<dyn AudioDevices>`:

```rust
pub enum AudioInitStatus { Undetermined, Denied, Granted }

pub enum AudioInitError {
    Denied,
    Undetermined,
    ActivationFailed(String),
}

/// A factory that brings up the OS audio session and yields a ready
/// AudioDevices. On non-iOS the session is a no-op and status is always
/// Granted.
pub trait AudioSessionProvider: Send + Sync {
    /// SYNC read of current permission state. No dialog, cheap.
    fn init_status(&self) -> AudioInitStatus;

    /// Fire the async OS permission request. Returns immediately. The
    /// sink is invoked LATER, on an arbitrary thread, carrying NO
    /// payload — it only signals "state may have changed, re-evaluate".
    /// On non-iOS, invokes the sink (granted) promptly through the same
    /// path so the park-and-resume flow is exercised everywhere.
    fn request(&self, sink: AudioPermissionSink);

    /// Bring up a working session and hand back a ready AudioDevices.
    /// Succeeds ONLY if status is Granted AND setActive(true) works.
    fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError>;
}
```

`AudioPermissionSink` is a `Send` wrapper over a cloned `Sender<Input>` +
a fixed generation; its only job is to enqueue an `Input::AudioPermissionResolved`.

### B2. The CQRS surface (head drives the prompt)

The internals **never auto-prompt**; only `request()` pops the dialog, and
only the head triggers it (after showing its "we're a singing coach, we
need the mic" framing). Two new commands, both replying with the **same**
event so the head reacts to one thing:

| New `Command` | Control-plane action | Reply event |
| --- | --- | --- |
| `AudioPermissionQuery` | `init_status()` (sync, no prompt) | `AudioPermissionStatus { status }` |
| `AudioPermissionRequest` | `request(sink)`; on resolve re-read `init_status()` | `AudioPermissionStatus { status }` |

New `CoachEvent::AudioPermissionStatus { status: AudioInitStatus }`.

At boot the head sends **both** (Query then Request); the in-order mailbox
drain guarantees the Query reply lands first. No startup broadcast (the
coach has no such pattern and an early broadcast races the head's drain).

### B3. Control-plane changes (`control_plane.rs`)

- **New `Input` variants:** `AudioPermissionResolved { generation: u64 }`
  (payload-free — the dumb sink enqueues this).
- **Generation counter** — control-thread-owned `u64`, **final semantics
  now** (so 1.6.1c is purely additive):
  - start accepted → increment; snapshot into the request sink + pending intent
  - stop accepted → bump (invalidate) + clear pending intent
  - `AudioPermissionResolved` processed only if `generation == current`; else dropped
- **`AudioPermissionRequest` handler:** snapshot generation, `request(sink)`.
- **`AudioPermissionResolved` handler:** if generation matches, re-read
  `init_status()` and emit `AudioPermissionStatus`. (Reconciliation reads
  live truth; the event carries no verdict — spec Decision 3.)
- **`do_start_session` (now `AudioStartSession`):**
  - `match init_status()`:
    - `Granted` → `new_devices()?` → enumerate → negotiate → build → open
      (fast path, no park). **Store the live `Box<dyn AudioDevices>`** for
      the session lifetime (dropping it deactivates the session).
    - `Denied` → `Error(PermissionDenied)` (terminal, no dialog).
    - `Undetermined` → `Error` (do **not** auto-prompt; the head owns
      prompting via `AudioPermissionRequest`). The head, seeing
      Undetermined, requests permission first, then starts.
  - `new_devices()` returning `ActivationFailed` → `Error` (start failure).
- **`do_list_devices` (now `AudioListDevices`):** if `init_status()` is
  `Granted`, `new_devices()?.list_devices()`; else emit
  `AudioDevicesListed { devices: [] }`. **Never prompts** (listing is
  passive; prompting is `AudioPermissionRequest`'s job alone).

### B4. New error kind

`AudioSessionErrorKind::PermissionDenied` (added now — b is the first
producer; the head consumes it in 1.6.1c to show "enable in Settings").

### B5. The iOS adapter (`adapter-audio-cpal`, `#[cfg(target_os = "ios")]`)

- New direct deps, iOS-gated: `objc2-avf-audio = "0.3"`, `objc2 = "0.6"`,
  `block2` (for the completion block), `objc2-foundation = "0.3"`.
- `AudioSessionProvider` impl:
  - `init_status` → `AVAudioSession.recordPermission()` mapped to the enum.
  - `request` → `setCategory(.record)`, then `requestRecordPermission(block)`.
    The block is an `RcBlock<dyn Fn(Bool)>` capturing **only** the `Send`
    sink (Sender + generation) — never the `!Send` session. It ignores the
    `Bool` and enqueues `AudioPermissionResolved { generation }`.
  - `new_devices` → assert `Granted`, `setActive(true)` (map failure to
    `ActivationFailed`), then build the existing cpal-backed `AudioDevices`.
- **Non-iOS default flavor:** `init_status` → `Granted`; `request` →
  invokes the sink immediately; `new_devices` → the current cpal devices.
  Keeps the Mac path flowing through the identical state machine.

### B6. Boot wiring (`apps/coach-game/src/main.rs`)

Construct the provider (not a ready `AudioDevices`) and put it in
`AppCoachDeps`. The ready devices are produced per-start on the control
thread via `new_devices()`.

---

## Testing — Mac/headless

A `FakeAudioSessionProvider` driving the state machine deterministically:

- `init_status` starts `Undetermined`; `request(sink)` resolves on command.
- `AudioPermissionQuery` → `AudioPermissionStatus{Undetermined}`.
- `AudioPermissionRequest` → parks; on resolve (status flipped to Granted)
  → `AudioPermissionStatus{Granted}`.
- `AudioStartSession` with `Granted` → straight to Running, no park.
- `AudioStartSession` with `Denied` → `Error(PermissionDenied)`, no prompt.
- `AudioStartSession` with `Undetermined` → `Error` (no auto-prompt).
- Stop during a pending request → cancels; a late `AudioPermissionResolved`
  with a stale generation is dropped (no unwanted start).
- `new_devices()` returning `ActivationFailed` → `Error`.

iOS simulator: best-effort manual (session activates, mic opens, negotiate
reads the live config). Real privacy prompt + on-device routes = 1.6.2.

## Explicitly NOT in 1.6.1b (→ 1.6.1c)

`LifecycleEvent` enum; the lifecycle-sink arg on `open()`; route /
interruption / `MediaServicesReset` handling; deadline-aware `run()` loop,
retry budget, debounce; head observation (Pause-with-Resume-locked). The
generation counter is built with its final semantics here; c only adds
call sites.
