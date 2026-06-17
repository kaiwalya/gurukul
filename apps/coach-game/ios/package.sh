#!/usr/bin/env bash
# Package coach-game as an iOS simulator .app bundle and install/launch it.
#
# Usage:
#   ./apps/coach-game/ios/package.sh           # debug build (default)
#   ./apps/coach-game/ios/package.sh --release # release build
#
# Output bundle: target/ios/coach-game.app  (relative to the repo root)
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

TARGET="aarch64-apple-ios-sim"
PROFILE="debug"
CARGO_FLAGS=()
BUNDLE_ID="com.gurukul.coach-game"

# ---------------------------------------------------------------------------
# Parse flags
# ---------------------------------------------------------------------------
for arg in "$@"; do
    case "$arg" in
        --release)
            PROFILE="release"
            CARGO_FLAGS+=(--release)
            ;;
        *)
            echo "Unknown flag: $arg" >&2
            exit 1
            ;;
    esac
done

BINARY_SRC="${REPO_ROOT}/target/${TARGET}/${PROFILE}/coach-game"
BUNDLE_OUT="${REPO_ROOT}/target/ios/coach-game.app"
PLIST_SRC="${SCRIPT_DIR}/Info.plist"
ASSETS_SRC="${SCRIPT_DIR}/../assets"

# ---------------------------------------------------------------------------
# Step 1: compile
# ---------------------------------------------------------------------------
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
# Step 3: install and launch on the simulator
# ---------------------------------------------------------------------------

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
xcrun simctl launch "${BOOTED_UDID}" "${BUNDLE_ID}"

echo "==> Done. coach-game is running in the simulator."
