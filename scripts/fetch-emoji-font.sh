#!/usr/bin/env bash
# The color emoji font the AppImage brings along: Noto Color Emoji in its
# bitmap (CBDT) form, which the AppImage's WebKit draws. Systems with only
# the COLRv1 form (Fedora) would crash that WebKit, so Carteiro hides that
# one (src-tauri/appimage-fonts.conf). Pinned by SHA-256; SIL OFL 1.1.
set -euo pipefail
OUT="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/fonts/NotoColorEmoji.ttf"
URL=https://github.com/googlefonts/noto-emoji/raw/v2.051/fonts/NotoColorEmoji.ttf
SHA256=72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b
mkdir -p "$(dirname "$OUT")"
if ! echo "$SHA256  $OUT" | sha256sum -c - >/dev/null 2>&1; then
  curl -fsSL --proto '=https' --retry 5 --retry-all-errors --retry-delay 10 --connect-timeout 30 -o "$OUT.part" "$URL"
  echo "$SHA256  $OUT.part" | sha256sum -c - >/dev/null || { echo "checksum mismatch: $URL" >&2; rm -f "$OUT.part"; exit 1; }
  mv "$OUT.part" "$OUT"
fi
echo "$OUT"
