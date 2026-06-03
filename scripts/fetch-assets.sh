#!/usr/bin/env bash
# Fetch dev-time assets that are not checked into git (license/size
# reasons). Run once before `cargo run -p coach-game`; idempotent.
#
# Today this is the Devanagari UI font. Bevy's embedded default font
# (FiraSans) has no Devanagari glyphs, so the Sargam-Devanagari note
# system renders tofu without it. The release/app bundle will embed the
# font properly; this script only covers the unbundled `cargo run` dev
# path.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fonts_dir="$repo_root/apps/coach-game/assets/fonts"
mkdir -p "$fonts_dir"

# Noto Sans Devanagari, SIL Open Font License 1.1, from Google Fonts.
font_url="https://github.com/google/fonts/raw/main/ofl/notosansdevanagari/NotoSansDevanagari%5Bwdth%2Cwght%5D.ttf"
license_url="https://github.com/google/fonts/raw/main/ofl/notosansdevanagari/OFL.txt"
font_out="$fonts_dir/NotoSansDevanagari.ttf"
license_out="$fonts_dir/NotoSansDevanagari-OFL.txt"

fetch() {
  local url="$1" out="$2" name="$3"
  if [[ -f "$out" ]]; then
    echo "✓ $name already present ($out)"
    return
  fi
  echo "↓ fetching $name …"
  curl -fsSL -o "$out" "$url"
  echo "✓ $name → $out"
}

fetch "$font_url" "$font_out" "Noto Sans Devanagari"
fetch "$license_url" "$license_out" "OFL license"
