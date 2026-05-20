# Phase 1.4.8 — Audio I/O preferences and live port inspection

Goal: turn the cabinet from "fixed pipeline, fixed I/O" into "user picks the I/O, user picks what to inspect." Two halves:

1. **The audio I/O preferences pane** — pick input device, pick output device, persist across launches.
2. **The debug pane** — pick any port in the live graph, see its output rendered with the right widget; if the port is audio, audition it through the output.

Phase 1.4 marked the cabinet *done*; this is the foundation polish before Phase 1.5 (interpretation). Naming it 1.4.8 — not 1.5 — keeps the phase boundary honest: this is live-surface work, not interpretation work.

Status: **✓ done.** All six PRs landed (1.4.8.1 → 1.4.8.6). User-facing surface: Cmd-, opens the preferences pane (input/output device + engine sample-rate pickers with engine rebuild + per-device alignment affordance); the main view has a "Debug pane" disclosure with node + port pickers and shape-appropriate widgets (Hz readout, event-magnitude bar, control readout, audio waveform with monitor toggle).

---

## 1. Why this earns a phase

Two things 1.4 didn't deliver but every later phase will want:

- **The debug pane is the first real consumer of the port-subscription pattern** described in [`ARCHITECTURE.md`](ARCHITECTURE.md) §"Port addressing and subscription". 1.4 hard-coded five outputs (`pitch`, `onset`, `breath`, `vibrato_rate`, `vibrato_depth`) at engine-build time. The debug pane is the first place a *user* picks the port at runtime — the same pattern Phase 1.5 rules, a future editor, and any game UI will inherit.
- **Output device routing has no consumer until now.** Building it on its own would be speculative; building it for the debug pane's audition feature gives it an immediate, testable user story.

The audio prefs pane is comparatively pedestrian — table-stakes UI every audio app ships — but the device-list-changed listener and persistence scaffolding it introduces will also serve every later phase.

---

## 2. Scope discipline — what this phase is *not*

- **Not sample-rate / buffer-size selection.** Both are hardcoded today (48 kHz, 4096-frame device buffer). Making them configurable requires rebuilding the engine on change, which touches lifecycle code we don't need to touch yet. Defer until something forces it.
- **Not a full preferences window (`Cmd-,`).** Inline disclosure section or a sheet attached to the main view is enough. A real preferences window is SwiftUI scaffolding for marginal benefit.
- **Not a multi-tap debug pane.** V1 is single-port-at-a-time. Side-by-side multi-tap is an obvious follow-up but stretches the phase.
- **Not feature-port sonification.** If you pick a feature port, you see a widget; you don't hear a synthesised tone. Pick an audio port to audition.
- **Not the ECS visualiser refactor.** The debug pane hardcodes the widget-per-port-type mapping. ECS-shaped "PortBinding component spawns the right view" is appropriate when there are dozens of views; we have five. Defer to 1.5 or later, as `PHASE_1_4.md` §7 originally scoped.

---

## 3. Architecture — what changes where

### Engine (Rust)

The engine already runs in the cabinet via `engine-ffi`. Two small gaps:

- **Runtime port enumeration.** `Engine` has `node_index()` and `topo_order()` but no way to ask "what output ports does node X have?" without going back to `NodeRegistry` and knowing the node's type. Add a public API that takes a node id and returns its port list. ([`engine/src/graph.rs:750`](../engine/src/graph.rs) `peek` is adjacent — same neighborhood.)
- **`peek` as the read primitive.** Already public, already realtime-safe-when-called-between-blocks, already documented as the right call for "give me this port's last block." The CLI's `--peek <node.port>` is the existing user. We re-use it for the cabinet — but rename or wrap it once we're sure of the shape (see §6).

### engine-ffi (Rust → C ABI)

Three new functions, all following the existing `engine_resolve_*` / `engine_in_port` / `engine_out_port` pattern at [`engine-ffi/src/lib.rs`](../engine-ffi/src/lib.rs):

```c
// Enumerate node ids in topo order. Returns count; fills caller's array.
// NOT realtime-safe (allocates on the Rust side). Call at engine-build
// or selection-change time, not per audio callback. Returned char*
// pointers are valid until engine_free.
size_t engine_node_ids(GurukulEngine*, const char** out, size_t cap);

// Enumerate output port names for a node.
// Same realtime / lifetime rules as engine_node_ids.
size_t engine_out_port_names(GurukulEngine*, const char* node_id,
                              const char** out, size_t cap);

// Read the last block's worth of samples from any node.port.
// Returns ptr+len like engine_out_port does today. Realtime-safe when
// called from the audio thread between process_block calls. Named
// `engine_read_port` (not `engine_peek`) so the cabinet binds to a
// stable FFI verb; the underlying Rust `peek` is a debug affordance
// today and may be replaced by the subscribe-by-path API named in
// ARCHITECTURE.md without renaming the FFI surface.
GurukulError engine_read_port(GurukulEngine*, const char* node_id,
                          const char* port, const float** ptr, size_t* len);
```

Header at [`engine-ffi/include/engine.h`](../engine-ffi/include/engine.h) gets the three new signatures, with the realtime-safety and lifetime contracts in the doc comments.

### Cabinet (Swift)

Three new files:

- **`SettingsView.swift`** — input/output picker, attached as a sheet or inline disclosure from `ContentView`.
- **`HALOutput.swift`** — counterpart to the input HAL path in `AudioEngine.swift`. Owns an `AudioDeviceCreateIOProcID` on the user-selected output device. Has one SPSC ring the audio thread writes (samples to play) and the HAL output thread reads (samples to render).
- **`DebugPaneView.swift`** — two pickers (node, port) bound to engine enumeration; a body that switches on port type and renders the matching widget; a *monitor* toggle (visible only when the selected port is audio) that routes the port's samples to the output device.

Changes to existing files:

- **`AudioEngine.swift`** — adds device enumeration helpers (input list, output list, list-changed listener), a runtime port-resolution layer for the user-selected debug port (parallel to the hard-coded five), and a write path into the output ring when audition is on.
- **`AudioPipeline.swift`** — exposes new published properties for "live node list" and "selected port", routes audition.
- **`ContentView.swift`** — adds a button/disclosure for Settings, a button/disclosure for the debug pane.
- **New: `Prefs.swift`** — thin wrapper around `UserDefaults`, keyed by device UID. Codable structs for the saved state.

### Persistence shape

Stored in UserDefaults (sandboxed plist at `~/Library/Containers/com.kaiwalya.Gurukul/Data/Library/Preferences/com.kaiwalya.Gurukul.plist`):

- `selected_input_device_uid: String?` — `nil` means follow system default.
- `selected_output_device_uid: String?` — `nil` means follow system default.

Debug-pane selections (node, port, monitor on/off, open/closed) are deliberately *not* persisted — every session starts with the pane closed and nothing selected. The pane is a debugging surface; sticky state across launches is more annoying than useful.

Keying by UID (not by name) is the standard so picking "my Scarlett 2i2" survives unplug/replug and name collisions. If the saved UID is no longer present at launch, fall back to system default and clear the key.

---

## 4. PR breakdown

Each PR is independently mergeable and produces a checkable artefact.

### PR 1.4.8.1 — Engine port enumeration

**What:** Add `pub fn out_port_names(&self, node_id: &str) -> Result<Vec<&str>, EngineError>` and `pub fn node_ids(&self) -> &[String]` to `Engine`. Add unit tests in `engine/tests/`.

**Why:** The debug pane needs a runtime answer to "what can I inspect?" Today that data exists internally (`output_port_names[node_idx]`) but isn't reachable from outside the crate.

**Scope:** pure additive. No node changes, no FFI yet.

**Input ports deliberately not exposed.** A parallel `in_port_names` is omitted: post-mux input buffers carry the same samples as their upstream output, and the upstream is already addressable via `out_port_names` on the source node. Adding `in_port_names` later if a real use case appears is mechanical.

**Done when:** `cargo test --workspace --release` passes, including a new test that builds a small world and asserts enumeration matches the hand-written world JSON.

### PR 1.4.8.2 — FFI for enumeration and peek

**What:** Add `engine_node_ids`, `engine_out_port_names`, `engine_read_port` to `engine-ffi`. Header declarations in `engine.h`. Add a Rust-side smoke test that builds a world through the FFI and round-trips a peek.

**Why:** Cabinet can't see these without FFI. Wrapping is mechanical given the existing pattern.

**Scope:** ~60 LOC of `extern "C"` glue + header lines + one smoke test.

**Done when:** New functions are callable from a small C harness OR from the cabinet's bridging header without warnings; smoke test passes.

### PR 1.4.8.3 — HAL output device path (dark)

**What:** New `HALOutput` Swift class. Mirrors `installHALInput()` but for output: discover the output device, install an `AudioDeviceCreateIOProcID` callback, claim a buffer-frame-size, own an SPSC ring the cabinet can write into. **The route is dark** — the ring is written with zeros, no audible output. Verification is structural, not audible: callback fires at the expected cadence, sample-clock advances correctly, clean teardown on app quit.

A manual-test-only "sidetone" toggle exists *only in a debug menu* (not a feature flag, not enabled in any shipped build). This is for the developer to sanity-check the path is alive end-to-end, not a user-facing feature. PR 5 introduces the first real consumer of the output ring.

**Why:** Output routing is the longest-pole technical piece, but shipping a sidetone-by-default PR creates feedback risk and a half-finished feature on `main` between PR 3 and PR 5. Ship the path dark; let PR 5 light it up against a user-selected port.

**Scope:** mostly Swift, no engine changes, no UI on the main view. The debug-menu sidetone toggle is a few lines, gated by a `#if DEBUG` or a developer-menu attribute.

**Done when:** Loopback test shows the output callback fires at the expected interval, sample-clock matches the input device's, app quit cleanly stops the device. Manual sidetone toggle (debug menu) produces audible passthrough with ~10ms latency for the developer's own verification, and is off by default on every launch.

### PR 1.4.8.4 — Audio preferences pane

**What:** `SettingsView.swift` with three pickers: input device, output device, **engine sample rate**. Backed by `Prefs.swift` (UserDefaults wrapper). Device pickers keyed by device UID. Sample-rate picker offers a fixed list (44.1k / 48k / 96k); default 48k.

New device-list-changed listener in `AudioEngine.swift` (currently only default-device-changed exists). On device pick, the cabinet stops the current HAL input/output, swaps in the new device, restarts. On sample-rate pick, the cabinet performs a **full engine rebuild**: stop input + output, `engine_free`, rebuild at the new rate via `engine_build`, re-resolve port handles, re-allocate sample-rate-sized scratch buffers, restart input + output. The rebuild path is new infrastructure that PR 5 (debug pane) will also use when a future world reload feature lands.

**Constraint:** input device, output device, and engine all run at the same rate. The picker does not resample — it aligns. If the user picks an output device whose native rate differs from the engine, the device picker surfaces this and offers to either (a) refuse, or (b) change the engine rate to match (one click). Same for input. The current behaviour of `HALOutput.start` (refuse with a clear log) becomes a UI message in the preferences pane.

**Why:** Sample-rate alignment is the main thing the user can't currently fix without `Audio MIDI Setup`. Folding it into the preferences pane keeps all alignment in one place and earns us the engine-rebuild plumbing that PR 5 will inherit for free.

**Scope:** all Swift on the cabinet side. Persistence is one extra key (`engineSampleRate: Int`). Hot-swap mechanics already proven for input by PR 6.2; engine-rebuild path is new. No engine / FFI changes — the engine already supports being built at arbitrary rates; we just rebuild the handle.

**Done when:**

- Pick a non-default input → preferences persist across app restart.
- Same for output.
- Pick a different sample rate → engine rebuild succeeds within ~1 s, pitch clock and waveform resume cleanly, no audio glitches, sidetone (if it was on) re-engages off (matches the existing reset-clears-sidetone invariant).
- Pick an output device whose native rate ≠ engine rate → preferences pane offers to align; one click rebuilds at the device's rate.
- Unplug the chosen device → falls back gracefully to system default with a status message.
- Re-plug → silently re-claims if "follow this UID" is set.

### PR 1.4.8.5 — Debug pane

**What:** `DebugPaneView.swift`. Two pickers (node, port) populated from FFI enumeration. Body switches on port type:

- **Audio port** → `WaveformView` (re-using the existing widget on a small slot driven by `engine_read_port`). Monitor toggle visible.
- **Feature port (Hz)** → `PitchTraceView` or a small live-Hz readout.
- **Feature port (event-shaped)** → `BreathStripView`-style tick row.
- **Control port** → numeric readout + sparkline.

Monitor toggle wires the selected port's samples (only when audio-typed) into the `HALOutput` ring from PR 3. Toggle hidden for non-audio ports.

**Why:** This is the payoff. The user's first time using the port-subscription pattern interactively.

**Scope:** Swift UI work + a new `DebugTapSlot` in `AudioPipeline` (triple-buffered SPSC, mirroring `FeatureSlot` / `WaveformSlot`). The audio thread calls `engine_read_port` once per hop on the user-selected port, then pushes the result into the slot. The UI tick reads the slot.

The slot's payload is **data, not a closure or callback**: `{ port_path: String, type_tag: PortShape, buffer: [Float] }`. This shape is deliberate — when the ECS visualiser refactor lands later, this is already a `PortBinding`-shaped value, and the refactor becomes mechanical (the slot's payload becomes a component on a debug-tap entity, the type_tag selects the view system).

**Invariants documented in this PR:**

- Debug selection does not survive an engine rebuild (a future world reload). On rebuild, `DebugTapSlot` clears, both pickers reset to nil, monitor toggle disengages. World reload doesn't exist yet, but this is the contract.
- Monitor toggle auto-disengages on `engine.reset()` (input device swap, world rebuild, any reset path). A click into the user's headphones at a route change is worse than silence.

**Done when:** Pick `node-pitch-yin.f0_hz` → see live pitch trace. Pick the world's mic input → see the waveform; toggle monitor → hear yourself. Pick `node-onset.events` → see ticks. Selection does *not* persist across app restart (intentional — see §3). Monitor disengages on engine reset (verified by swapping input device with monitor on).

### PR 1.4.8.6 — ROADMAP / PHASE_1_4 doc update

**What:** Update `ROADMAP.md`'s "Current phase" line to reflect that 1.4.8 happened. Add a one-paragraph note to `PHASE_1_4.md` §7 acknowledging that the ECS deferral still stands but that 1.4.8 built the debug pane on the simpler (hardcoded widget-per-type) shape.

**Why:** Project status should be honest about what shipped.

**Scope:** doc-only.

---

## 5. Risks and unknowns

- **Threading discipline for `engine_read_port`.** Must be called from the audio thread, immediately after `process_block` returns — same as the existing five `engine_out_port` reads. The result is pushed into a triple-buffered SPSC slot owned by `AudioPipeline`; the UI thread reads the slot, never the engine. Calling `engine_read_port` directly from the UI tick would race against `process_block` on the audio thread. PR 5 adds one new debug-tap slot following the existing `FeatureSlot` / `WaveformSlot` pattern.
- **Enumeration functions allocate.** `engine_node_ids` and `engine_out_port_names` allocate on the Rust side and are not realtime-safe. Cabinet calls them at engine-build and on debug-pane picker open, never per audio callback. Documented in the header doc comments (PR 2).
- **Monitor route survival across resets.** Swapping the input device or rebuilding the engine while monitor is on would push a click of stale samples through the output. Monitor auto-disengages on `engine.reset()` — invariant added to PR 5's done-criteria.
- **Output feedback loop.** Monitoring the mic input through speakers (not headphones) is a feedback risk. The monitor toggle should warn or be hidden when output and input share the same physical hardware path. At minimum, a clear status message.
- **HAL output for non-default devices.** Mirroring `installHALInput()` for output should be straightforward but we haven't done it before. Allow time for the first round of CoreAudio surprises (similar to the 200 ms tap lag we hit on input).
- **Sample-rate mismatch.** If the chosen output device is locked to 44.1 kHz and the engine runs at 48 kHz, we need a resampler at the cabinet seam. **Out of scope for V1** — for now, refuse to engage output devices whose nominal sample rate doesn't match the engine. Pop a status message. Real resampling is a follow-up.
- **UserDefaults vs JSON file.** We're going UserDefaults for now. Switching later is a one-day migration if it becomes painful.

---

## 6. Open questions to settle before PR 1

1. **Naming for the FFI peek function.** `engine_read_port` matches the Rust method but ties us to that name. Alternatives: `engine_read_port`, `engine_port_buffer`. Ship `engine_read_port` for consistency; rename if the eventual production read API lands a different verb.
