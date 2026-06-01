# Working in `apps/coach-game/`

The Bevy host for the singing-coach. A *head* in the hexagonal sense —
it wires the same adapters as `coach-cli` into an `AppCoach` and
renders a souls-like UX on top. Targets Mac today, iOS later (same
codebase, different build configuration).

Module layout: `coach` owns the `!Send` AppCoach handle as a NonSend
resource and the always-on event drain. `state` defines `AppState`
(`MainMenu` / `Settings` / `InGame`) and the `SelectedDevice` /
`KnownDevices` resources. `menu::main_menu` and `menu::settings` are
the menu screens. `game` runs the session: `StartSession` on enter,
`StopSession` on exit, feature snapshots logged each frame.

## Running it

`cargo run -p coach-game`. **It takes ~3 seconds to boot.** Bevy
initializes wgpu, opens a winit window, and only then starts logging
from `bevy_winit::system: Creating new window …`. If you smoke-test
with a `sleep` + `kill`, give it **at least 6 seconds** — anything
shorter and you'll kill it before the window appears, before mic
permission resolves, and before any feature snapshots arrive. The
binary is unbundled (`cargo run` launches the raw executable, not a
`.app`), so its window usually opens *behind* the terminal and won't
steal focus; check Mission Control / `Cmd-Tab` if you don't see it.

Bevy logs to **stderr**, not stdout. Capture with `>file 2>&1` (or
just `2>&1 | tee file`) when smoke-testing — redirecting only stdout
gives you an empty log file and a misleading "no output" conclusion.

## Conventions

- Bevy 0.18. Bundles are gone — `Node` is a component, not
  `NodeBundle`; spawn with `(component, component, ...)` tuples. Use
  `children![ ... ]` for hierarchies. Helpers `px(150)`, `percent(100)`
  replace `Val::Px` / `Val::Percent`. Events were renamed to messages:
  `MessageWriter<AppExit>` + `.write(...)`, not `EventWriter::send`.
- State-scoped despawn uses `DespawnOnExit(state)` (the state type
  opts in via `#[states(scoped_entities)]`). Don't reach for the old
  `StateScoped`.
- `#![allow(clippy::type_complexity)]` is set crate-wide — Bevy
  queries trip the lint by construction.
- One file per screen under `menu/`. When a screen grows beyond
  ~200 lines, split *inside* its module rather than flattening across
  the menu/ tree.

## Project-wide rules

[`../../AGENTS.md`](../../AGENTS.md).
