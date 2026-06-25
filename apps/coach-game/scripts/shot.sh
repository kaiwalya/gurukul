#!/usr/bin/env bash
# Capture a screenshot of the running coach-game window, by window ID.
#
# Why by ID: coach-game is an unbundled raw binary (no .app), so it is NOT
# AppleScript-scriptable — `System Events ... front window` returns nothing,
# and a fixed `screencapture -R x,y,w,h` region drifts out of alignment.
# CoreGraphics' on-screen window list *does* see it (owner "coach-game"),
# giving a stable window ID that `screencapture -l<id>` crops exactly.
# Needs only Screen Recording permission (already granted to the terminal);
# no AppleScript automation permission, no extra installs (swift ships with
# the Xcode CLT).
#
# Usage:  apps/coach-game/scripts/shot.sh [output.png] [owner-name]
#   output.png  defaults to ./coach-game-shot.png
#   owner-name  defaults to "coach-game" (the process owner in the window list)
set -euo pipefail

OUT="${1:-coach-game-shot.png}"
OWNER="${2:-coach-game}"

WID="$(swift - "$OWNER" <<'SWIFT' 2>/dev/null | head -1
import CoreGraphics
import Foundation
let target = CommandLine.arguments.count > 1 ? CommandLine.arguments[1].lowercased() : ""
guard let list = CGWindowListCopyWindowInfo(
    [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]]
else { exit(1) }
for w in list {
    let owner = (w[kCGWindowOwnerName as String] as? String) ?? ""
    if owner.lowercased().contains(target) {
        print((w[kCGWindowNumber as String] as? Int) ?? -1)
    }
}
SWIFT
)"

if [ -z "${WID:-}" ]; then
  echo "shot.sh: no on-screen window owned by '$OWNER' — is the app running?" >&2
  exit 1
fi

screencapture -o -l"$WID" "$OUT"
echo "$OUT"
