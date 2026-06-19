# Plan: Phase 1.6 — phone port (iOS first)

Recompile the Rust core + the same Bevy head for phone, ship Stage 1 on
a device. The roadmap entry is the scope of record; this plan is the
*how*, grounded in a spike that already ran our actual app on the iOS
simulator.

iOS goes first (the dev machine is a Mac; the simulator + toolchain are
already in hand). Android follows the same hexagonal pattern once iOS is
shipping.

## Spike outcomes

A spike compiled and ran the **real `coach-game` head** (not a Bevy
sample) on the iOS 26 simulator (iPhone 17 Pro). What it established:

**Works as-is — no change needed:**

| Area | Result |
| --- | --- |
| Toolchain | Xcode 26.4.1 + simulators already installed; only the `rustup` iOS targets were missing (`aarch64-apple-ios`, `aarch64-apple-ios-sim`) — added, no admin/GUI step. |
| Compile | The whole head built clean for the simulator **and** the real-device target on the first try. No dependency in our stack fights iOS. |
| Render | wgpu picks the Metal simulator GPU, winit creates a `UIWindowScene`, the Bevy render loop runs at full rate. |
| UI | After the window fix below, the **menu** and the **InGame surface** (note dial + scrolling time-graph + HUD) both render full-screen. |
| Session logic | The session state machine runs on iOS — Sa grid + scale degrees computed correctly on entering Free Practice. |
| Audio adapter | `adapter-audio-cpal` compiles + links for the device target; cpal pulls its iOS backend (`coreaudio-rs` → `objc2-audio-toolbox`/`objc2-avf-audio` = AudioUnit + AVAudioSession). Same code path as macOS — **no new capture adapter needed.** |

**One fix already landed during the spike:**

- **Blank-render → fixed.** On iOS winit creates the window at **0×0**
  and never reflows; the whole UI laid out into a zero rect, so the
  screen stayed black while the render loop spun (confirmed: ~14k
  `RedrawRequested` in 90 s, zero pixels). Fix: a platform-gated
  `ios_window()` gives the live window `BorderlessFullscreen` on iOS;
  Mac/replay untouched. Committed (`fix(coach-game): give the iOS window
  a fullscreen mode so UI renders`).

**Three gaps the spike surfaced — these are the work of this phase:**

1. **Mic never opens (root-caused).** Entering Free Practice shows **no
   permission dialog** and the session lands in `Error`:
   `build_input_stream → UnsupportedConfig: device no longer available`.
   Device-log corroboration: `AQMEDevice has neither a
   defaultInStreamClient`, and **no TCC mic request** is ever made. iOS
   requires an **`AVAudioSession`** configured (category `.record` /
   `.playAndRecord`) and **activated**, plus a runtime record-permission
   request, *before* cpal touches the AudioUnit. We skip that entirely.
   The Mac mic is already routed into the sim (`Guest Audio Input →
   Default Host Audio Device`), so once the session is set up, sim +
   real device both capture through the same cpal path.

2. **No touch way out of a session.** Back-to-menu (InGame → Paused) is
   bound to `KeyCode::Escape` only. A phone has no Esc and there's no
   on-screen control, so a session is a one-way trap on touch (the
   user hit this directly and had to kill the simulator). Menu *buttons*
   already work via tap — they're Bevy UI `Button`s.

3. **Portrait squeezes the layout.** The InGame layout is a horizontal
   row (`graph_slot` + `dial_slot` side by side); it is a landscape
   design. iOS defaults to portrait, which crushes it into vertical
   slivers. One Info.plist key fixes it — verified in the spike bundle.

**Minor:** trace recording writes to a relative `traces/` dir, which
fails on the iOS read-only sandbox (`Read-only file system`). This hits
**both** the audio-trace recorder *and* the UX trace wired in `main.rs`
— neither is fatal, but both fault until the path is rooted under the
app's writable container (Documents/Caches/tmp) or recording is skipped
on iOS.

**Tooling note for anyone re-running the spike:** `simctl io screenshot`
captures the device's **native portrait buffer** regardless of physical
rotation, so landscape content looks sideways in the saved PNG even when
the live Simulator window is correct. Trust the on-screen Simulator, not
the screenshot file, for orientation.

## What does *not* change

The hexagonal split holds: the engine, all four analyzers, the
`AppCoach` port, the session state machine, the tuning/scale model, and
every read-model resource are reused untouched. This phase is **head +
adapter glue only** — no DSP, no domain logic.

## Phases

Ordering revised after review: the bundle/plist scaffold comes **first**
(mic and orientation both live in Info.plist, so testing them on a
hand-assembled bundle is testing the assembly, not the app), and a
real-device mic smoke test is pulled **early** (simulator success does
not imply device success — the device adds the privacy prompt, hardware
route, interruptions, and signing/plist validation).

### Phase 1.6.0 — Bundle + plist scaffold

Replace the hand-assembled spike `.app` with a reproducible,
source-controlled bundle (still simulator-targeted, no signing yet).
This is the foundation every later test stands on. The bundle carries
the real Info.plist: `NSMicrophoneUsageDescription`,
`UISupportedInterfaceOrientations` (landscape), device family, launch
screen. Decide the build harness — **Xcode shell project** (a thin
native target that links the Rust staticlib via a cargo build phase) vs.
a **cargo-driven bundle** (a script that assembles the `.app` directly,
extending what the spike did). **Done when:** `<one command>` produces a
launchable simulator bundle from a clean tree, with the production
Info.plist baked in.

### Phase 1.6.1 — Mic on iOS (AVAudioSession + full lifecycle)

The core blocker, and bigger than "configure before cpal opens." iOS
needs an **`AVAudioSession`** configured (category `.record` /
`.playAndRecord`) and **activated**, plus a runtime record-permission
request — and that session governs more than the first open:

- **Enumeration may depend on the session too.** `audio-cpal` lists
  devices / picks a default before capture; on iOS `default_input()` /
  `supported_input_configs()` may be empty or stale without an active
  record-capable session. Set up the session **before both** device
  enumeration and `build_input_stream`, not only before opening capture.
- **Let the session dictate format.** The app currently requests a fixed
  ~10 ms buffer (`sample_rate / 100`, `BufferSize::Fixed`) and prefers a
  chosen sample rate. iOS often imposes its own IO buffer duration and
  session sample rate. Fall back to `BufferSize::Default` (or map
  desired latency via `setPreferredIOBufferDuration`), and **verify the
  negotiated rate/channel count after open** — rebuild the engine if the
  actual cpal stream differs from the requested config.
- **Lifecycle, not one-shot.** Wire iOS notifications to the existing
  engine reset/stop hooks: permission denied/restricted, interruption
  begin/end (calls, Siri), and route changes (headphones in/out). On
  interruption or route change, stop → reactivate session → reopen
  stream → reset engine. Don't crash on app quit while a stream is live.

iOS-specific glue gated by `#[cfg(target_os = "ios")]`, behind the
audio-capture adapter where possible. **Done when:** entering Free
Practice prompts for mic permission; after granting, the pitch trace +
dial track the Mac mic in the simulator; and an interruption (simulated)
recovers cleanly.

### Phase 1.6.2 — Early real-device mic smoke

Before any touch/orientation polish, prove the mic on **physical
hardware** — this is where the privacy prompt, hardware route, and
plist/entitlement validation are real. Requires **Apple Developer
signing** (the user's Apple ID / provisioning) — the first step that
needs the user, not just the agent. Keep it minimal: build, sign,
install, enter Free Practice, confirm the device's own mic drives the
trace. **Done when:** the app captures real mic input on an iPhone.

### Phase 1.6.3 — Touch input parity ✅ (landed as 1.6.2c)

The on-screen pause/back affordance shipped: a top-right pause button in
InGame opens the existing Paused menu on tap (Esc kept for desktop), so a
session is exitable by touch and recordings get a stop trigger. Remaining
audit item: confirm no *other* keyboard-only control exists in the touch
round trip.

### Phase 1.6.4 — Orientation, layout & trace paths

Confirm the landscape lock (Info.plist key from 1.6.0) renders the
horizontal InGame layout correctly on device. Root **both** trace paths
(audio-trace + the UX trace in `main.rs`) under the writable container,
or skip recording on iOS. **Done when:** the app launches landscape,
the dial + time-graph lay out correctly, and no read-only-filesystem
faults appear.

### Phase 1.6.5 — Operational hardening

The iOS-specific failure modes a desktop app never sees. Smoke-check:
cold launch; permission **denied → retry → granted**; app
background/foreground; call/Siri-style interruption; route change
(headphones); app quit while the stream is active. Surface mic errors
**in-UI** rather than sitting silently in `Error` state. **Done when:**
each scenario behaves predictably with no crash.

### Phase 1.6.6 — Android

Repeat the pattern for `aarch64-linux-android`: same head cross-compiled,
per-platform mic session plumbing behind the capture port. Sequenced
after iOS ships so the iOS work informs it. (Detailed once iOS lands.)

## Top risks (and mitigations)

- **AVAudioSession × cpal lifecycle is the real porting risk.** Not just
  "session before open" — enumeration, buffer/sample-rate negotiation,
  and interruption/route recovery all route through the session
  (see 1.6.1). *Mitigation:* follow cpal's `ios-feedback` example +
  `coreaudio-rs` PR #72 ordering; let the session dictate format and
  verify post-open; wire interruptions to the existing engine reset
  hook. The spike proved the backend *links* — this phase proves it
  *runs*.
- **Simulator success ≠ device success.** The device adds the privacy
  prompt, hardware route, interruptions, and signing/plist validation.
  *Mitigation:* the early real-device smoke (1.6.2) catches this before
  polish is sunk on top of a sim-only assumption.
- **Bevy fights a target (input/audio session/app-store).**
  *Mitigation:* the hexagonal escape hatch — swap only that platform's
  head for a native one (SwiftUI/Compose), `AppCoach` reused. Not
  expected on iOS given the spike, but it's why the port stays thin.
- **Signing/provisioning friction.** *Mitigation:* keep simulator-based
  iteration (no signing) as the default dev loop through 1.6.0–1.6.1;
  device signing is a gated step at 1.6.2.

## Open issues (harvested from completed sub-plans)

Done so far: **1.6.0** (bundle), **1.6.1/1.6.1b/1.6.1c** (mic + lifecycle),
**1.6.2a** (permission UX), **1.6.2c** (on-screen pause button), plus the
sim-trace / telemetry-file / recorder-finalize infra. The detailed plans for
those were deleted once landed; the *open* items they named are folded here so
nothing is lost.

**Carry-overs that still need doing:**

- **On-device mic smoke (was 1.6.2).** Sim works; physical hardware is unproven.
  Needs Apple-ID signing/provisioning — the first user-gated step. The
  `TODO(1.6.2)` markers left in the cpal `open()` path (route/interruption
  observers) get exercised here. *Done when:* the app captures real mic input on
  an iPhone.
- **Pitch-graph jitter.** A latent erratic-needle/graph artefact, reproducible
  on Mac via `--replay-audio` over a recovered sim WAV (`.scratch/repros/`). Not
  platform-specific — it's in the captured signal or the pitch pipeline
  (YIN/smoothing/projection). Diagnose before going to device so device-side
  weirdness is unambiguous.
- **`MediaServicesReset` recoverable rebuild** (was "terminal in 1.6.1"). On-
  device only; the head currently exits to `Error` on this. → fold into 1.6.2/1.6.5.
- **"Reconnecting" affordance** for silent recovery cycles (RouteChanged etc.).
  Polish, → 1.6.5. 1.6.1 only landed exit-on-terminal-error.
- **Surface mic errors in-UI** (a spinner/affordance vocabulary), rather than
  sitting silently in `Error`. → 1.6.5.
- **Mac CoreAudio route/unplug feed** for the lifecycle seam — works with the
  fake source today; the real-listener feed is a small follow-up.

**Smaller follow-ups (do only if a need appears):**

- Auto-pull the iOS trace bundle off the sim (manual recipe in `BUILD.md` for now).
- `--replay`/`--replay-audio` against sim-captured bundles (deferred at capture time).
- Swipe/edge gesture to pause (the button covers the need; gestures cost
  TouchInput tracking + discoverability).
- Neutral `RunRecording` struct / shared trace-prefix extraction — only "if a
  third consumer appears."

## Explicitly deferred

- Android (until iOS ships — Phase 1.6.6 is a stub here on purpose).
- App-store packaging, icons, launch screens, TestFlight.
- Any UI redesign for small screens beyond the landscape lock — Stage 1
  ships the existing layout; touch-first visual rework is later.
- Stage 2+ features (interpretation, spectral, etc.) — out of phase.

## Provenance

Findings in **Spike outcomes** are from a live simulator run, not
inference. The render fix is committed; the three gaps and the minor
trace-path issue are tracked as tasks. Per project workflow, this plan
goes to Codex for review before implementation begins.
