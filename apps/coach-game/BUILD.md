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

For **debugging** on each platform — pulling traces off the sim and device,
and reading the device system log — see
[`PLATFORM-DEBUGGING.md`](PLATFORM-DEBUGGING.md).

- **Targets:** `aarch64-apple-ios-sim` (simulator) and `aarch64-apple-ios`
  (device). Added via `rustup target add`; no Xcode GUI step.
- **Packaging step — today (Phase 1.6.0):** a plain `xcrun`-based script that
  copies the compiled binary into a `.app`, bakes in a checked-in
  `Info.plist`, and installs to the simulator **unsigned**. The plist carries
  `NSMicrophoneUsageDescription`, the orientation lock
  (`UISupportedInterfaceOrientations`, landscape), device family, and bundle
  id/version. Done when one command produces a launchable simulator bundle
  from a clean tree.

  **One command (run from the repo root):**
  ```
  ./apps/coach-game/ios/package.sh             # debug
  ./apps/coach-game/ios/package.sh --release   # release
  ```
  The script: compiles via `cargo build`, assembles the bundle at
  `target/ios/coach-game.app` (binary + `Info.plist` from `ios/Info.plist` +
  `assets/` from `apps/coach-game/assets/`), then `xcrun simctl
  install`/`launch` on the booted simulator (boots iPhone 16 Pro if none is
  running).

  Plist source of truth: [`ios/Info.plist`](ios/Info.plist).

### iOS — device install (Phase 1.6.2)

Build, sign, and install on a physical iPhone (run from the repo root):

```
./apps/coach-game/ios/package.sh --device              # debug
./apps/coach-game/ios/package.sh --device --release    # release
```

> **Fresh checkout?** Read the one-time setup below first. The repo
> deliberately ships **no** signing material — Apple requires a personal
> code-signing certificate and a provisioning profile to put an app on a real
> iPhone, and both are tied to *your* Apple ID, so they can't live in git
> (the profile is gitignored; the cert lives in your macOS keychain). You
> supply them once; after that the command above just works.

**Per-run prerequisites (device side):**
- Developer Mode on the iPhone (Settings → Privacy & Security → Developer Mode).
- Device trusted on this Mac (connect via USB, accept "Trust this computer").

#### One-time setup: cert + provisioning profile

You need two things Apple gates behind your account:
- a **code-signing certificate** ("Apple Development: …") in your login keychain, and
- a **provisioning profile** — a signed file binding *(your team + this app's
  bundle id + your registered device + that cert)*. It's the permission slip
  iOS checks at install time.

Both require a **paid** Apple Developer account ($99/yr). The free
personal-team path (7-day re-sign) is *not* wired into this script.

**Easiest path — let Xcode mint both for you.** Even though this project has
no `.xcodeproj`, Xcode is still the simplest way to get the cert + profile
onto your machine:

1. Xcode → **Settings → Accounts → +** → sign in with your Apple ID, select
   your team. This alone creates and installs the **certificate** in your
   keychain (no CSR dance).
2. Plug in the iPhone and let Xcode register it (Window → **Devices &
   Simulators** shows it; "Use for Development" registers its UDID with your
   team).
3. Create the **profile** for bundle id `com.kaiwalya.gurukul.game` — quickest
   via the portal (next paragraph), or let any throwaway Xcode app target with
   that bundle id + "Automatically manage signing" generate it, then grab the
   `.mobileprovision` Xcode downloaded from
   `~/Library/Developer/Xcode/UserData/Provisioning Profiles/`.

**Or, by hand on [developer.apple.com](https://developer.apple.com):**
Identifiers → register the App ID `com.kaiwalya.gurukul.game` with **no
capabilities** (the mic needs none — `NSMicrophoneUsageDescription` in
[`ios/Info.plist`](ios/Info.plist) plus the runtime prompt is the whole
requirement). → Devices → register the iPhone by UDID (if it nags *"update
your device list for the new membership year,"* clear that first or the
device won't be selectable). → Profiles → **iOS App Development** → pick the
App ID + cert + device → download the `.mobileprovision`.

**Then put your config in a `.env`.** The repo ships **no** personal signing
values — copy the template at the repo root and fill in *yours*:

```sh
cp .env.example .env          # repo root; .env is gitignored
```

| Variable in `.env` | What | Find yours with |
| --- | --- | --- |
| `GURUKUL_IOS_IDENTITY` | codesign identity | `security find-identity -v -p codesigning` |
| `GURUKUL_IOS_DEBUG_DEVICE` | target device id (optional — auto-picks the sole connected device) | `xcrun devicectl list devices` |
| `GURUKUL_IOS_PROFILE` | path to your `.mobileprovision` (optional) | the file you downloaded above |

The script reads `.env` through [**dotenvx**](https://dotenvx.com) (`brew
install dotenvx/brew/dotenvx`) — it re-execs itself under `dotenvx run` when
the signing vars aren't already set — so once `.env` is filled in, the
`--device` command just works. If you'd rather not set `GURUKUL_IOS_PROFILE`,
you can instead pass `--profile <path>` or drop the file at
`apps/coach-game/ios/profile.mobileprovision` (also gitignored).

> The device id from `devicectl` is CoreDevice's id — **not** the same string
> as the UDID the developer portal asks for (that one's also in the
> `devicectl` listing, or in Finder by clicking the device's serial).

Under the hood the script embeds the profile, extracts its entitlements,
`codesign`s the `.app`, then `devicectl` installs and launches. The recurring
`"No provider was found." Code=1002` lines during install/launch are
**harmless noise** — both still succeed.

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

> **Physical device note:** `simctl get_app_container` only works for simulator
> installs. On a physical device the trace bundle is reachable via
> `devicectl device copy from` — see
> [`PLATFORM-DEBUGGING.md`](PLATFORM-DEBUGGING.md) → "Physical device".

On the sim the trace root resolves to `<sandbox-home>/Documents/traces/` —
writable by the app, readable by you off disk. The sandbox path is
**stable across shutdown** but changes on reinstall or erase, so resolve it
per session while the sim is booted:

```sh
# Resolve the data container (sim must be booted; bundle id from ios/Info.plist)
DATA=$(xcrun simctl get_app_container booted com.kaiwalya.gurukul.game data)

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
