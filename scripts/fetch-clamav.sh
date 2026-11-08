#!/bin/sh
# Downloads the official ClamAV build for a platform, verifies its SHA-256
# (pinned below, from the GitHub release), and lays it out in
# src-tauri/resources/clamav/ for bundling:
#   clamd[.exe] freshclam[.exe] lib/ (Linux) *.dll (Windows) certs/ COPYING*
# Usage: scripts/fetch-clamav.sh linux-x64|windows-x64
# Linux also needs patchelf (sets the bundled programs' library path).
# ClamAV is GPL-2.0 and ships as separate programs; see THIRD-PARTY.md.
set -eu
VERSION=1.5.4
case "${1:-}" in
  linux-x64)   FILE="clamav-$VERSION.linux.x86_64.deb"; SHA=28d6efc5b4423e7830c3559339552eb53870a9eac51ac4efb37d60530d329886 ;;
  windows-x64) FILE="clamav-$VERSION.win.x64.zip";      SHA=0d9e0228b2674137ea1a2853566c98a0278ad52ab2582c3d6dbd75373848c395 ;;
  *) echo "usage: $0 linux-x64|windows-x64" >&2; exit 2 ;;
esac
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/src-tauri/resources/clamav"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

curl -fsSL --proto '=https' --retry 5 --retry-all-errors --retry-delay 10 --connect-timeout 30 -o "$WORK/$FILE" "https://github.com/Cisco-Talos/clamav/releases/download/clamav-$VERSION/$FILE"
echo "$SHA  $WORK/$FILE" | sha256sum -c -

rm -rf "$OUT"
mkdir -p "$OUT"
case "$1" in
  linux-x64)
    (cd "$WORK" && ar x "$FILE" && mkdir x && tar -xzf data.tar.gz -C x)
    L="$WORK/x/usr/local"
    cp "$L/sbin/clamd" "$L/bin/freshclam" "$OUT/"
    mkdir -p "$OUT/lib" "$OUT/certs"
    # Real files under their sonames (installers don't always keep symlinks).
    for so in libclamav.so.12 libclammspack.so.0 libclamunrar.so.12 libclamunrar_iface.so.12 libfreshclam.so.4; do
      cp -L "$L/lib/$so" "$OUT/lib/$so"
    done
    cp "$L"/etc/certs/* "$OUT/certs/"
    # The official build looks for its libraries in /usr/local/lib. Point
    # the programs at the bundled lib/ next to them, and the libraries at
    # each other, so they work from any folder (and AppImage bundling can
    # resolve them).
    command -v patchelf >/dev/null || { echo "patchelf is required" >&2; exit 1; }
    patchelf --set-rpath '$ORIGIN/lib' "$OUT/clamd" "$OUT/freshclam"
    for so in "$OUT"/lib/*.so*; do patchelf --set-rpath '$ORIGIN' "$so"; done
    find "$L/share/doc" -iname 'COPYING*' -exec cp {} "$OUT/" \; 2>/dev/null || true
    ;;
  windows-x64)
    (cd "$WORK" && unzip -q "$FILE" -d x)
    SRC="$(dirname "$(find "$WORK/x" -iname clamd.exe | head -1)")"
    cp -R "$SRC"/. "$OUT/"
    ;;
esac
echo "ClamAV $VERSION ready in $OUT"
ls "$OUT"
