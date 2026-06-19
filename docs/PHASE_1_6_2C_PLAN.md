# Plan: an on-screen pause button — reach the exit on touch (Phase 1.6.2c)

**Goal:** make the *existing* pause/exit flow reachable on iOS. Today the only
way to leave a Free Practice session is the **Escape key** (`handle_esc_in_game`,
`game/mod.rs:211`), which touch devices don't have — so the fully-built `Paused`
screen (Resume / Quit-to-menu) is dead on mobile. Add a single on-screen
**pause button** in the in-game HUD that does exactly what Escape does:
`AppState → Paused`.

This also gives recordings a real **stop trigger** on iOS (entering `Paused`
fires `OnExit(InGame)` → `Command::AudioStopSession` → recorder seal), which is
why it pairs with the finalize work.

## What exists (verified, file:line)

| Piece | Where |
| --- | --- |
| `AppState` enum incl. `Paused` | `state.rs:13-25` |
| Escape → Paused (the behavior to mirror) | `game/mod.rs:211-222` `handle_esc_in_game`: sets `Paused`, `HasPausedSession=true`, `ResumeLocked=false` |
| Pause screen (Resume / Settings-disabled / Quit-to-menu w/ confirm) | `menu/paused.rs`; wired at `lib.rs:214-228` |
| Session stops on leaving InGame | `OnExit(InGame)` → `game/mod.rs:180-196` sends `Command::AudioStopSession` |
| In-game top strip (where the button goes) | `game/mod.rs:57-59` `hud_slot` (a bare `Node{}`), added as first child of `in_game` root (`:132`); root has `padding left:32 right:0 top:24` (`:121-128`) |
| Existing top-left element in that strip | the HUD badge — `widgets/hud/systems.rs:25-52` `spawn()`, parented into `hud_slot` |
| Button→handler pattern to copy | `menu/main_menu.rs:126-135` `handle_settings`: marker + `Query<&Interaction, (Changed<Interaction>, With<Marker>)>` + on `Pressed` mutate state |
| Design tokens (colors, px helper, font sizes) | `src/ui.rs`: `COLOR_TEXT` (:17), **`COLOR_TEXT_DIM` (:18) — use this for the dim resting glyph**, `FONT_HEADER/BODY` (:22-24), `px()`. No `percent()` helper exists → use `Val::Percent(100.0)` directly for the strip width. |

**Key fact driving the anti-graze treatment:** pausing **ends the take** — it
stops the session and resume starts a *fresh* one (`state.rs` doc on `Paused`;
`on_enter` issues a new `StartSession`). So an accidental tap interrupts a
performance (recoverable via Resume, but it cuts the recording). Hence inset the
button from the corner.

## Design (settled with ui-designer)

| Aspect | Decision |
| --- | --- |
| Form | **icon-only** pause glyph `⏸` (or two bars `❚❚` text) — reads as "control"; the left badge stays text. No label. |
| Position | **top-right** of the top strip, balancing the top-left scale badge |
| Treatment | **borderless**, dim resting state (~40–50% of `COLOR_TEXT`) so it recedes during singing. Echo the badge's vertical alignment, differ in weight. |
| Touch target | **≥44 px** hit area (Apple HIG); visible glyph smaller inside the target |
| Anti-graze | **inset from the corner** — give the strip/button real right padding (mirror the 24/32 insets) so it's a deliberate reach, not a palm graze in landscape |

## Changes

### 1. Top strip becomes a space-between row

`game/mod.rs` `spawn_root` — `hud_slot` (`:57-59`) is today a bare `Node{}`
holding only the badge (left). Make it lay out left+right:
```rust
Node {
    width: percent(100),
    flex_direction: FlexDirection::Row,
    justify_content: JustifyContent::SpaceBetween,
    align_items: AlignItems::FlexStart,
    // right inset: the root has right:0, so the button needs its own
    // breathing room from the screen edge (anti-graze). Mirror the 32 used
    // on the left, or use the existing `px(24)` rhythm.
    padding: UiRect { right: px(32), ..default() },
    ..default()
}
```
The HUD badge already parents itself into `hud_slot` (left child); the pause
button becomes the right child. (Confirm the badge spawn doesn't assume it's the
*only* child — it returns its own entity id, so adding a sibling is safe.)

### 2. New pause-button widget + handler

Smallest footprint: put it in the `game/` route glue, mirroring how the HUD is
spawned and parented (the route owns slot partitioning — see `game/mod.rs`
slot markers and the route-owns-partition rule).

- **Marker:** `#[derive(Component)] struct PauseButton;`
- **Spawn:** a `Button` + `Node` (≥44px square hit area, e.g. `width/height
  px(44)`, `justify_content/align_items: Center`) with a `Text::new("⏸")` child
  (or two-bar glyph) using `TextColor` dimmed from `COLOR_TEXT` and an
  appropriate `FONT_*`. Parent it into `hud_slot` as the right child (alongside
  the badge), in the same place `spawn_root`/`on_enter` wires the HUD.
- **Handler:** copy `handle_settings` exactly (`main_menu.rs:126-135`) but for
  `PauseButton`, and instead of `next.set(Settings)`, replicate
  `handle_esc_in_game`'s body: `next.set(AppState::Paused)`, set
  `HasPausedSession = true`, `ResumeLocked = false`. **Factor the shared body**
  into one helper (e.g. `fn request_pause(next, has_paused, resume_lock)`) called
  by BOTH `handle_esc_in_game` and the new button handler — so Escape and tap
  stay in lockstep and the pause semantics live in one place. Register the new
  handler in `Update` while `in_state(InGame)` (next to `handle_esc_in_game` at
  `lib.rs:178`).

### 3. Despawn

The button lives under `InGameRoot` (via `hud_slot`), which already carries
`DespawnOnExit(AppState::InGame)` (`game/mod.rs:110`), so it's cleaned up on
exit automatically — no extra despawn wiring.

## Explicitly NOT in this slice

- No change to `paused.rs` (the pause menu itself is done).
- No gestures / swipe-to-pause (deferred; discoverability + TouchInput tracking
  cost not justified when a button works).
- No confirm-on-pause (designer: too heavy; Resume makes pause recoverable, and
  Quit already confirms). The inset is the chosen mitigation.
- No "resume continues the same recording" — pause ends the take by design;
  changing that is a separate, larger feature.
- Don't touch the HUD badge's own behavior (it still opens the scale picker).

## Verify

1. `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
   `cargo test --workspace --release` clean.
2. **Test:** extend `tests/state_transitions.rs` (it already exercises
   InGame↔Paused, but via `NextState` directly — see the `// bypassing the Esc`
   helper near `:272`, NOT a key-press sim). For the button, prefer the most
   robust of: (a) set the `PauseButton` entity's `Interaction` to `Pressed`, run
   `app.update()`, assert `InGame → Paused` + `HasPausedSession.0 == true`; or
   (b) if driving `Interaction` in a headless test is awkward, unit-test the
   shared `request_pause` helper directly (call it, assert it sets state +
   flags). Either proves tap and Esc share one path. Keep existing tests green.
3. **Mac smoke:** run `coach-game`, enter Free Practice, click the top-right
   button → pause menu appears; Resume returns to a fresh session; the Escape
   key still works identically.
4. **Sim proof:** on the iOS sim, tap the button → pause menu; confirm a trace
   WAV is sealed at that point (the stop trigger now fires on touch).
