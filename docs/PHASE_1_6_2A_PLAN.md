# Plan: Phase 1.6.2a — mic permission UX (audio flows in Free Practice)

Step "a" of Phase 1.6.2. **End goal (user's words):** tap *Free Practice*
on the iOS simulator → if the mic isn't granted, ask → on grant, the
session opens and **audio actually flows**. Today it doesn't, by design:
the permission *machinery* shipped in 1.6.1b, but no head UI ever triggers
the prompt, so `AudioStartSession` errors on `Undetermined` and the device
list is blank. This step builds the missing UI.

The *why* and the cross-cutting permission model live in
[`PHASE_1_6_1_SPEC.md`](PHASE_1_6_1_SPEC.md) (Decision 1–2). This file is
the settled *how* for the UX, after a design pass with the UI designer.

## What already exists (verified, not assumed)

| Piece | Where | State |
| --- | --- | --- |
| Permission machinery (`AudioPermissionQuery`/`Request` → `AudioPermissionStatus`) | control plane + `AudioDriver` port | ✅ 1.6.1b |
| `init_status()` → `Undetermined`/`Denied`/`Granted` | `audio_driver` port | ✅ |
| iOS `AVAudioApplication` prompt wiring | `adapter-audio-cpal` driver | ✅ 1.6.1b |
| `do_list_devices` returns **empty** when not `Granted` (never prompts) | `control_plane.rs:487` | ✅ — this is why Settings → Audio is blank |
| Free Practice → straight to `AudioStartSession` | `menu/main_menu.rs:91`, `game/mod.rs:169` | ⚠️ no permission fork |
| Quit-confirm modal (overlay pattern to reuse/extract) | `menu/paused.rs` (`ConfirmModalRoot`/`ShowingQuitConfirm`/`COLOR_OVERLAY`) | ✅ |
| `AppLifecycle` message (foreground/background) | `bevy_window` 0.18 | ✅ available |
| `ButtonDisabled` greying vocabulary | `ui.rs` + `paused.rs` | ✅ |

**Key fact:** one mic grant unlocks *both* surfaces — Free Practice audio
*and* the Settings device list. They were both blank for the same reason.

## The permission model (settled — not redesigned here)

Three states the UI reacts to:

| `init_status()` | Meaning | UI action |
| --- | --- | --- |
| `Undetermined` | never asked | show panel + **Allow Microphone** (pops OS prompt) |
| `Denied` | said no — iOS never re-prompts | show panel + **Open Settings** (route to iOS Settings) |
| `Granted` | yes | start / list devices |

`Denied` always means "go to iOS Settings"; no 4th state. On Mac the
driver is always `Granted`, so neither panel ever paints there.

---

## The design — one content, two presentations

A single stateless helper draws the permission **content** (headline +
body + action button); two callers wrap it differently.

```
spawn_permission_panel(commands, parent, status)
  ├── modal caller   (Free Practice)  → overlay backdrop + Cancel/Back
  └── inline caller  (Settings Audio) → fills the tab, no backdrop
```

Shared *content*, separate *lifecycle* (the modal has a backdrop +
dismiss; the inline version has neither). One helper, two callers — NOT
one entity tree serving both.

### Copy (terse — souls-like, dyslexia-friendly)

| State | Headline (`FONT_HEADER`) | Body (`FONT_BODY`, dim) | Button |
| --- | --- | --- | --- |
| Undetermined | `Microphone needed` | `Gurukul listens to you sing.` | `Allow Microphone` |
| Denied | `Microphone is off` | `Turn it on in Settings to sing.` | `Open Settings` |

No icon, no spinner, no accent-color button (accent is reserved for
title/selection). Hierarchy comes from order + copy.

---

## Surface 1 — Free Practice fork (modal over the menu)

Tapping the start button forks on `init_status()`:

| Status | Action |
| --- | --- |
| `Granted` | `NextState(InGame)` immediately — current behavior |
| `Undetermined` | spawn modal with the Undetermined panel |
| `Denied` | spawn modal with the Denied panel |

**A state machine, not a flag** (per Codex review). The prompt resource is
an explicit enum, NOT `Option<status>` — it must distinguish *waiting*,
*checking hardware*, and *which intent* drove a `Granted`:

```rust
enum PermissionPrompt {
    Hidden,                       // no modal
    NeedsPermission(AudioInitStatus), // Undetermined or Denied panel shown
    RequestPending,               // OS sheet open; button = Waiting…
    CheckingHardware,             // Granted; AudioListDevices in flight
    NoHardware,                   // Granted but fresh list empty → block
}
```

A `Changed<PermissionPrompt>` sync system spawns/despawns/morphs the modal.
**React to the resource, not the click** — the click only *sends commands*;
status/device replies advance the resource, which is the single source of
truth both surfaces read.

**The async transitions:**

1. Tap **Allow Microphone** → send `Command::AudioPermissionRequest`,
   set `RequestPending` → button greys to `Waiting…`.
2. OS sheet is modal on iOS (occludes our UI); the disabled state covers
   the frame before/after.
3. `AudioPermissionStatus` reply → advance the resource:
   - → `Denied`: morph to `NeedsPermission(Denied)` in place (rebuild
     children like `rebuild_device_list`); backdrop stays; button →
     `Open Settings`.
   - → `Granted`: **do not enter yet** — set `CheckingHardware`, send a
     fresh `Command::AudioListDevices` (see no-hardware below).
4. **Cancel/Back** (second button, modal only): → `Hidden`, stay on menu.

**Granted-but-no-hardware (user: block) — fresh async handshake** (Codex
#3/#4): `KnownDevices` may be **stale** (left over from Settings or a denied
state) and the list arrives async, so the start fork must NOT consult the
cached list. Instead: on `Granted`, enter `CheckingHardware` and send a
fresh `AudioListDevices`; when *that* `AudioDevicesListed` reply lands,
decide:
- non-empty → despawn modal, `NextState(InGame)`.
- empty → `NoHardware`: panel body `No microphone found.`, only a `Cancel`.

Caveat (Codex #4): `do_list_devices` returns `vec![]` for *both* real
no-device *and* `setActive` activation failure — so `No microphone found.`
is the generic block copy for "granted but unusable," accepted as-is for
1.6.2a (a richer list error is deferred).

## Surface 2 — Settings → Audio (inline)

Same content, rendered in the tab where the device list goes:

| Status | Tab content |
| --- | --- |
| `Undetermined` | panel + `Allow Microphone`, inline |
| `Denied` | panel + `Open Settings`, inline |
| `Granted` | the device list (current behavior) |
| `Granted`, no hardware | one dim line `No microphone found.` instead of the list |

The tab's existing Back button is the dismiss; no backdrop.

**Default-row gate (Codex #5):** the device list today always renders a
`System default` row (`audio.rs:83`), so an empty list never *visibly*
reads as no-hardware. Gate the default row behind `Granted && non-empty`
so "no hardware" actually shows the `No microphone found.` line.

---

## The foreground re-query (settled: wire it now)

iOS only reflects a permission change (player fixed it in iOS Settings and
returned) if we re-check on resume. Add a system reading
`MessageReader<AppLifecycle>`; on **`AppLifecycle::WillResume` only** (NOT
also `Running` — querying both double-fires, Codex #2), re-send
`Command::AudioPermissionQuery`. The head-side mic-status resource updates
**only when the value changes** (idempotent — a duplicate
`AudioPermissionStatus` is a no-op), so the surfaces' `Changed<>` syncs
don't re-fire spuriously. Closes the Denied → Settings → return loop
without a relaunch.

---

## The modal helper extraction (settled: extract)

Two modals now (quit-confirm + permission) ⇒ extract
`spawn_overlay_modal(commands, parent, …)` (backdrop + centered body +
button row) into `ui.rs`. Refactor `paused.rs`'s quit-confirm to use it,
then build the permission modal on it. Touches working quit-confirm code —
re-verify its behavior after.

---

## Mechanical change list

| Where | Change |
| --- | --- |
| `ui.rs` | new `spawn_overlay_modal(...)` helper (backdrop + body + button row) |
| `menu/paused.rs` | refactor quit-confirm to use `spawn_overlay_modal` |
| `menu/permission.rs` (new) | `spawn_permission_panel(status)` content helper; `PermissionPrompt` resource; modal sync system; button handlers (Allow / Open Settings / Cancel) |
| `menu/main_menu.rs` | start button → fork on status instead of unconditional `NextState(InGame)` |
| `menu/settings/audio.rs` | when not `Granted` (or no hardware), render the inline panel instead of the device list |
| `coach.rs` | head-side mic-status resource updated from `AudioPermissionStatus`; `AppLifecycle` foreground → re-send `AudioPermissionQuery` |
| `state.rs` | `PermissionPrompt` + the mic-status resource registration |
| iOS — "Open Settings" | open `UIApplication.openSettingsURLString` via objc2 (iOS-gated; Mac = no-op / never shown) |

## Testing

- **Mac (headless/run):** Mac driver is always `Granted` → both surfaces
  fall through to current behavior (no modal, device list shows). Regression
  guard: Free Practice still enters InGame; Settings still lists devices.
- **iOS sim (the real win):** fresh install → tap Free Practice → our
  panel → `Allow Microphone` → **real OS prompt** → grant → audio flows.
  Deny → panel morphs to Denied → `Open Settings`. Fix in iOS Settings →
  return → foreground re-query → device list populates.
- **Logic unit tests** where feasible (status fork, no-hardware block). Head
  Bevy systems are compile-time-checked (no heavy harness invented), per the
  1.6.1c precedent.

## Open questions resolved (by the user)

1. Foreground re-query — **wire it now.**
2. Granted-but-no-hardware — **block** with `No microphone found.`
3. Modal helper — **extract** `spawn_overlay_modal`.
4. Ask placement — Settings gets an inline `Ask Permission`/`Open Settings`
   affordance (the user's framing: "the inside of the dialog, rendered in
   the menu").

## Explicitly NOT in 1.6.2a (deferred)

On-device signing/provisioning; real hardware route/interruption testing
(needs the 1.6.2 `AVAudioSession` observers — the `TODO(1.6.2)` left in
`open()`); a "reconnecting" affordance (1.6.5); a spinner primitive; an
icon vocabulary; per-device-permission nuance beyond the three states.
