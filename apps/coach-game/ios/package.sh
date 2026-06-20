#!/usr/bin/env bash
# Package coach-game as an iOS .app bundle and install/launch it.
#
# Usage:
#   ./apps/coach-game/ios/package.sh                        # sim, debug (default)
#   ./apps/coach-game/ios/package.sh --release              # sim, release
#   ./apps/coach-game/ios/package.sh --device               # device, debug
#   ./apps/coach-game/ios/package.sh --device --release     # device, release
#   ./apps/coach-game/ios/package.sh --device --profile <path>  # explicit profile
#   ./apps/coach-game/ios/package.sh --autostart            # sim: boot into InGame directly
#   ./apps/coach-game/ios/package.sh --autostart --autokill 10  # sim: boot in, kill after 10s (small traces)
#
# Output bundle: target/ios/coach-game.app  (relative to the repo root)
#
# Device signing config (required in --device mode). Set these as environment
# variables, or put them in the gitignored root `.env` (see `.env.example`).
# No values are baked into this script — they are personal to your Apple ID:
#   GURUKUL_IOS_PROFILE   path to the .mobileprovision file
#   GURUKUL_IOS_IDENTITY  codesign identity, e.g. "Apple Development: NAME (TEAMID)"
#   GURUKUL_IOS_DEBUG_DEVICE    devicectl device id (xcrun devicectl list devices)
#
# Packaging doctrine (BUILD.md):
#   cargo compiles the binary; this script does everything else.
#   Sign/entitlement lines go here when Phase 1.6.2 arrives — no restructuring.
set -euo pipefail

# ---------------------------------------------------------------------------
# Paths — all computed relative to this script so CWD doesn't matter.
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

# Personal signing config (GURUKUL_IOS_*) is read from the environment, loaded
# from the gitignored root `.env`. If those vars aren't already set and dotenvx
# is available with a root `.env`, re-exec ourselves under `dotenvx run` so the
# values are injected — works the same whether invoked directly or by hand.
# Nothing personal is baked into this script. See
# .env.example. (Set the vars yourself to bypass dotenvx entirely.)
if [[ -z "${GURUKUL_IOS_DOTENV_LOADED:-}" && -z "${GURUKUL_IOS_IDENTITY:-}" \
      && -f "${REPO_ROOT}/.env" ]] && command -v dotenvx >/dev/null 2>&1; then
    export GURUKUL_IOS_DOTENV_LOADED=1
    exec dotenvx run -f "${REPO_ROOT}/.env" -- "${BASH_SOURCE[0]}" "$@"
fi

TARGET="aarch64-apple-ios-sim"
PROFILE="debug"
CARGO_FLAGS=()
BUNDLE_ID="com.kaiwalya.gurukul.game"
MODE="sim"
PROVISION_PROFILE=""
AUTOSTART=""
AUTOKILL=""

# ---------------------------------------------------------------------------
# Parse flags
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)
            PROFILE="release"
            CARGO_FLAGS+=(--release)
            shift
            ;;
        --device)
            MODE="device"
            TARGET="aarch64-apple-ios"
            shift
            ;;
        --profile)
            if [[ $# -lt 2 ]]; then
                echo "ERROR: --profile requires a path argument" >&2
                exit 1
            fi
            PROVISION_PROFILE="$2"
            shift 2
            ;;
        --autostart)
            AUTOSTART="--autostart"
            shift
            ;;
        --autokill)
            if [[ $# -lt 2 ]]; then
                echo "ERROR: --autokill requires a seconds argument" >&2
                exit 1
            fi
            AUTOKILL="$2"
            shift 2
            ;;
        *)
            echo "Unknown flag: $1" >&2
            exit 1
            ;;
    esac
done

BINARY_SRC="${REPO_ROOT}/target/${TARGET}/${PROFILE}/coach-game"
BUNDLE_OUT="${REPO_ROOT}/target/ios/coach-game.app"
PLIST_SRC="${SCRIPT_DIR}/Info.plist"
ASSETS_SRC="${SCRIPT_DIR}/../assets"

# ---------------------------------------------------------------------------
# Device mode: resolve provisioning profile and signing identity
# ---------------------------------------------------------------------------
if [[ "${MODE}" == "device" ]]; then
    # Resolve the profile SOURCE: --profile flag > GURUKUL_IOS_PROFILE.
    # The source of truth lives outside the repo (your .env points at it).
    # ios/profile.mobileprovision is a gitignored *intermediary artifact* — we
    # (re)stage it from the source every run, so it can never drift.
    PROFILE_SRC="${PROVISION_PROFILE}"
    if [[ -z "${PROFILE_SRC}" ]]; then
        PROFILE_SRC="${GURUKUL_IOS_PROFILE:-}"
    fi
    STAGED_PROFILE="${SCRIPT_DIR}/profile.mobileprovision"
    if [[ -n "${PROFILE_SRC}" ]]; then
        if [[ ! -r "${PROFILE_SRC}" ]]; then
            echo "ERROR: provisioning profile not readable: ${PROFILE_SRC}" >&2
            exit 1
        fi
        # Re-stage from source (skip the copy if src IS the staged file).
        if [[ "$(cd "$(dirname "${PROFILE_SRC}")" && pwd)/$(basename "${PROFILE_SRC}")" != "${STAGED_PROFILE}" ]]; then
            cp "${PROFILE_SRC}" "${STAGED_PROFILE}"
        fi
    elif [[ ! -r "${STAGED_PROFILE}" ]]; then
        echo "ERROR: device mode requires a provisioning profile." >&2
        echo "  Set GURUKUL_IOS_PROFILE in .env (the source of truth), or pass" >&2
        echo "  --profile <path>. See .env.example / BUILD.md." >&2
        exit 1
    fi
    PROVISION_PROFILE="${STAGED_PROFILE}"
    # Signing identity and target device are personal to your Apple ID; they
    # must come from the environment (loaded from root .env by the cargo aliases,
    # or exported yourself). No defaults are baked in. See .env.example.
    if [[ -z "${GURUKUL_IOS_IDENTITY:-}" ]]; then
        echo "ERROR: GURUKUL_IOS_IDENTITY is not set (codesign identity)." >&2
        echo "  Copy .env.example to .env and fill it in, or export it." >&2
        echo "  Find yours with: security find-identity -v -p codesigning" >&2
        exit 1
    fi
    SIGN_IDENTITY="${GURUKUL_IOS_IDENTITY}"
    # Target device: GURUKUL_IOS_DEBUG_DEVICE if set, else auto-pick the sole
    # connected device. It only disambiguates which device to install onto, so
    # with exactly one plugged in it's optional. Erroring on 0 or many keeps it
    # unambiguous.
    DEVICE_ID="${GURUKUL_IOS_DEBUG_DEVICE:-}"
    if [[ -z "${DEVICE_ID}" ]]; then
        # Pull the UUID-shaped Identifier from each `connected` row (skips
        # `available (paired)` watches, etc.). Matching the UUID shape avoids
        # brittle column-counting.
        mapfile -t _devs < <(
            xcrun devicectl list devices 2>/dev/null \
                | awk '/connected/ {
                    for (i = 1; i <= NF; i++)
                        if ($i ~ /^[0-9A-Fa-f]{8}-([0-9A-Fa-f]{4}-){3}[0-9A-Fa-f]{12}$/)
                            print $i
                  }'
        )
        if [[ "${#_devs[@]}" -eq 1 ]]; then
            DEVICE_ID="${_devs[0]}"
            echo "==> Using sole connected device ${DEVICE_ID}"
        elif [[ "${#_devs[@]}" -eq 0 ]]; then
            echo "ERROR: no connected device found." >&2
            echo "  Plug in/unlock the iPhone, or set GURUKUL_IOS_DEBUG_DEVICE." >&2
            echo "  See: xcrun devicectl list devices" >&2
            exit 1
        else
            echo "ERROR: ${#_devs[@]} devices connected; set GURUKUL_IOS_DEBUG_DEVICE" >&2
            echo "  to choose one. See: xcrun devicectl list devices" >&2
            exit 1
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Step 1: compile
# ---------------------------------------------------------------------------
# Pin the iOS deployment target. Without this, the device link defaults to
# arm64-apple-ios10.0.0, which is too old to provide modern libSystem symbols
# (e.g. ___chkstk_darwin) that the prebuilt object files reference — the link
# then fails with "symbol(s) not found for architecture arm64". Not personal,
# so it lives here (not .env); only the real-device target needs it, but
# setting it unconditionally is harmless for the simulator.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-14.0}"

echo "==> cargo build -p coach-game --target ${TARGET} ${CARGO_FLAGS[*]+"${CARGO_FLAGS[*]}"}"
(cd "${REPO_ROOT}" && cargo build -p coach-game --target "${TARGET}" "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}")

# Validate the binary actually landed.
if [[ ! -f "${BINARY_SRC}" ]]; then
    echo "ERROR: binary not found at ${BINARY_SRC}" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Step 2: assemble the .app bundle
# ---------------------------------------------------------------------------
echo "==> Assembling bundle at ${BUNDLE_OUT}"
rm -rf "${BUNDLE_OUT}"
mkdir -p "${BUNDLE_OUT}"

# Binary
echo "    copying binary"
cp "${BINARY_SRC}" "${BUNDLE_OUT}/coach-game"

# Info.plist (source of truth: ios/Info.plist, checked in)
echo "    copying Info.plist"
cp "${PLIST_SRC}" "${BUNDLE_OUT}/Info.plist"

# Assets — Bevy resolves asset root to the executable's own dir on iOS.
echo "    copying assets"
cp -r "${ASSETS_SRC}" "${BUNDLE_OUT}/assets"

# Validate the plist is well-formed before attempting install.
echo "    validating Info.plist"
plutil -lint "${BUNDLE_OUT}/Info.plist"

echo "==> Bundle contents:"
ls -lh "${BUNDLE_OUT}"

# ---------------------------------------------------------------------------
# Step 2b (device only): sign the bundle
# ---------------------------------------------------------------------------
if [[ "${MODE}" == "device" ]]; then
    echo "==> Signing bundle for device"

    echo "    embedding provisioning profile"
    cp "${PROVISION_PROFILE}" "${BUNDLE_OUT}/embedded.mobileprovision"

    echo "    extracting entitlements"
    # macOS mktemp only substitutes XXXX when it is the trailing template; a
    # ".plist" suffix after it is taken literally (and then collides on rerun).
    # Make the temp with the X's trailing, then add the .plist extension.
    ENTITLEMENTS="$(mktemp /tmp/gurukul-ent.XXXXXX).plist"
    security cms -D -i "${PROVISION_PROFILE}" | plutil -extract Entitlements xml1 -o "${ENTITLEMENTS}" -

    echo "    codesigning ${BUNDLE_OUT}"
    codesign --force --sign "${SIGN_IDENTITY}" \
        --entitlements "${ENTITLEMENTS}" \
        --timestamp=none \
        "${BUNDLE_OUT}"

    echo "    verifying signature"
    codesign --verify --verbose "${BUNDLE_OUT}"

    echo "==> Bundle signed."
fi

# ---------------------------------------------------------------------------
# Step 3: install and launch
# ---------------------------------------------------------------------------
if [[ "${MODE}" == "device" ]]; then
    echo "==> Installing on device (${DEVICE_ID})"
    xcrun devicectl device install app --device "${DEVICE_ID}" "${BUNDLE_OUT}"

    echo "==> Launching ${BUNDLE_ID}"
    # devicectl forwards trailing args to the app; --autostart boots into InGame.
    # Capture the launched PID so --autokill can terminate it afterwards.
    LAUNCH_JSON="$(mktemp)"
    # shellcheck disable=SC2086
    xcrun devicectl device process launch --terminate-existing \
        --device "${DEVICE_ID}" --json-output "${LAUNCH_JSON}" \
        "${BUNDLE_ID}" ${AUTOSTART}

    if [[ -n "${AUTOKILL}" ]]; then
        APP_PID="$(/usr/bin/python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["result"]["process"]["processIdentifier"])' "${LAUNCH_JSON}" 2>/dev/null)"
        echo "==> Auto-kill in ${AUTOKILL}s (pid ${APP_PID:-unknown})"
        sleep "${AUTOKILL}"
        if [[ -n "${APP_PID}" ]]; then
            xcrun devicectl device process terminate --device "${DEVICE_ID}" --pid "${APP_PID}" 2>/dev/null || true
            echo "==> Terminated pid ${APP_PID} after ${AUTOKILL}s."
        else
            echo "==> Could not read PID; skipped terminate. App may still be running."
        fi
    else
        echo "==> Done. coach-game is running on the device."
    fi
    rm -f "${LAUNCH_JSON}"
else
    # Find a booted simulator, or boot a sensible default.
    BOOTED_UDID=$(xcrun simctl list devices booted --json \
        | python3 -c "
import json, sys
devices = json.load(sys.stdin)['devices']
for runtime, devs in devices.items():
    for d in devs:
        if d.get('state') == 'Booted':
            print(d['udid'])
            sys.exit(0)
")

    if [[ -z "${BOOTED_UDID}" ]]; then
        echo "==> No booted simulator found — booting iPhone 16 Pro"
        DEVICE_UDID=$(xcrun simctl list devices available --json \
            | python3 -c "
import json, sys
devices = json.load(sys.stdin)['devices']
# Prefer iPhone 16 Pro, fall back to any iPhone
for runtime, devs in sorted(devices.items(), reverse=True):
    for d in devs:
        if 'iPhone 16 Pro' in d.get('name', ''):
            print(d['udid'])
            sys.exit(0)
for runtime, devs in sorted(devices.items(), reverse=True):
    for d in devs:
        if 'iPhone' in d.get('name', '') and d.get('isAvailable', False):
            print(d['udid'])
            sys.exit(0)
sys.exit(1)
")
        echo "    booting ${DEVICE_UDID} (waiting until fully booted)"
        # `simctl boot` returns once boot is *initiated*; installing against a
        # still-booting device races and fails intermittently. `bootstatus -b`
        # boots if needed, then blocks until the device is fully booted.
        xcrun simctl bootstatus "${DEVICE_UDID}" -b
        BOOTED_UDID="${DEVICE_UDID}"
    fi

    echo "==> Opening Simulator.app"
    open -a Simulator

    echo "==> Installing bundle (UDID: ${BOOTED_UDID})"
    xcrun simctl install "${BOOTED_UDID}" "${BUNDLE_OUT}"

    echo "==> Launching ${BUNDLE_ID}"
    # shellcheck disable=SC2086
    xcrun simctl launch "${BOOTED_UDID}" "${BUNDLE_ID}" ${AUTOSTART}

    if [[ -n "${AUTOKILL}" ]]; then
        # Automated test runs: let the app live ${AUTOKILL}s (long enough to
        # bring up audio + emit a few feature snapshots), then terminate so the
        # UX/audio traces stay small. The app finalizes its gzip trace on a
        # clean terminate, so the latest run remains readable.
        echo "==> Auto-kill in ${AUTOKILL}s"
        sleep "${AUTOKILL}"
        xcrun simctl terminate "${BOOTED_UDID}" "${BUNDLE_ID}" 2>/dev/null || true
        echo "==> Terminated ${BUNDLE_ID} after ${AUTOKILL}s."
    else
        echo "==> Done. coach-game is running in the simulator."
    fi
fi
