# Building `coach-game` per target

*How the head compiles and packages for each platform.* For the crate's
internal shape see [`ARCHITECTURE.md`](ARCHITECTURE.md); for how to build a
widget see [`CONTRIBUTING.md`](CONTRIBUTING.md). Per-OS *runtime* quirks
(audio session, permissions, interruptions) live with the adapter that owns
them, not here — this doc is the **build & packaging pipeline**.

## The principle: cargo compiles, a post-cargo step packages

Every target follows the same split, and it is load-bearing:

> **Cargo's only job is to compile the Rust head to a binary for the target.
> Everything platform-specific lives in a packaging step that runs *after*
> cargo, never inside it.**

The packaging step grows **monotonically** — the same step gains capability
over time, it is never rebuilt:

```
cargo build (binary)  →  package: bundle + manifest  →  package: sign  →  package: store submission
                         └──────────────── one packaging step, more lines over time ───────────────┘
```

Why the split: cargo stays portable and unaware of Apple/Google tooling, and a
target's packaging step can advance from "unsigned local install" to "store
submission" without ever touching the build or the source.

## macOS

The default dev loop. `cargo run -p coach-game` launches the **raw unbundled
binary** — no `.app`, no packaging step at all (boot time, log capture, and
trace mechanics are in [`AGENTS.md`](AGENTS.md)). Mac needs no packaging until
it has its own distribution story, which it does not yet.

## iOS

Same codebase, different build configuration. Cargo cross-compiles the head;
a packaging step assembles the bundle.

- **Targets:** `aarch64-apple-ios-sim` (simulator) and `aarch64-apple-ios`
  (device). Added via `rustup target add`; no Xcode GUI step.
- **Packaging step — today (Phase 1.6.0):** a plain `xcrun`-based script that
  copies the compiled binary into a `.app`, bakes in a checked-in
  `Info.plist`, and installs to the simulator **unsigned**. The plist carries
  `NSMicrophoneUsageDescription`, the orientation lock
  (`UISupportedInterfaceOrientations`, landscape), device family, and bundle
  id/version. Done when one command produces a launchable simulator bundle
  from a clean tree.

  **One command (from any CWD inside the repo):**
  ```
  cargo ios            # debug
  cargo ios-release    # release
  ```
  These are cargo aliases (`.cargo/config.toml`) that shell out to
  `apps/coach-game/ios/package.sh`. The script: compiles via `cargo build`,
  assembles the bundle at `target/ios/coach-game.app` (binary +
  `Info.plist` from `ios/Info.plist` + `assets/` from
  `apps/coach-game/assets/`), then `xcrun simctl install`/`launch` on the
  booted simulator (boots iPhone 16 Pro if none is running).

  Plist source of truth: [`ios/Info.plist`](ios/Info.plist).
- **Packaging step — later:** **signing / provisioning / entitlements**
  (Phase 1.6.2, first step needing an Apple ID) and eventually **app-store
  submission** are *more lines in the same script* — not a new build system,
  not something cargo learns about. If signing friction ever justifies a thin
  Xcode target (a native target that links the Rust staticlib), that is a
  conscious swap of the packaging step, not the default. Note 1.6.2 also
  gives the script a **device installer arm** — the `aarch64-apple-ios`
  target plus `devicectl`/`ios-deploy` instead of `simctl` — alongside the
  signing lines; the assemble/install steps are already separated so this
  slots in as a target branch, not a rewrite.

> Decision of record (Phase 1.6.0): a cargo-driven `xcrun` script over an
> Xcode shell project, because signing is deferred and an `.xcodeproj` buys
> nothing until then. Revisit only if device signing makes the script unwieldy.

## Retrieving trace bundles from the iOS simulator

On the sim the trace root resolves to `<sandbox-home>/Documents/traces/` —
writable by the app, readable by you off disk. The sandbox path is
**stable across shutdown** but changes on reinstall or erase, so resolve it
per session while the sim is booted:

```sh
# Resolve the data container (sim must be booted; bundle id from ios/Info.plist)
DATA=$(xcrun simctl get_app_container booted com.gurukul.coach-game data)

# List trace files
ls "$DATA/Documents/traces/"

# Inspect a specific bundle (example; substitute your stamp)
gzcat "$DATA/Documents/traces/<stamp>-ux.jsonl.gz" | jq .
```

All four files from a run share the same stamp:
- `<stamp>-engine-input.wav`
- `<stamp>-engine-input.features.jsonl`
- `<stamp>-engine-input.manifest.json`
- `<stamp>-ux.jsonl.gz`

Shutting down the sim does **not** delete the container. Re-installing the app
or erasing the sim will assign a new container UUID — re-run `get_app_container`
in that case.

## Android

Deferred to Phase 1.6.6 (after iOS ships, so the iOS work informs it). Same
doctrine: `aarch64-linux-android` cross-compiled by cargo, a post-cargo step
producing the APK/AAB and handling the manifest, permissions, and signing.
Detailed once iOS lands.
