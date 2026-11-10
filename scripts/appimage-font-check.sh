#!/usr/bin/env bash
# The AppImage's own WebKit must draw color emoji, in color, without its web
# process dying: it aborts on COLRv1 fonts such as Fedora's Noto-COLRv1,
# which left Carteiro blank. Draws a page of emoji off screen with the bundled
# libraries, the system's fonts plus a pinned COLRv1 emoji font (as on
# Fedora), and the font setup Carteiro applies in the AppImage
# (src-tauri/appimage-fonts.conf, with the AppImage's own emoji font).
#
#   scripts/appimage-font-check.sh path/to/squashfs-root
# Needs gcc and the WebKitGTK 4.1 headers; without a display, xvfb-run.
set -euo pipefail
ROOT=$(realpath "$1")
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

test -f "$ROOT/usr/share/carteiro/fonts/NotoColorEmoji.ttf" || { echo "::error::the AppImage has no emoji font"; exit 1; }

FONT_URL=https://github.com/googlefonts/noto-emoji/raw/v2.051/fonts/Noto-COLRv1.ttf
FONT_SHA256=0ae57fe58645638523ba35f388d93739d292539a9acb84df5700c81b1e1a28d2
mkdir -p "$WORK/fonts"
curl -fsSL --proto '=https' --retry 3 -o "$WORK/fonts/Noto-COLRv1.ttf" "$FONT_URL"
echo "$FONT_SHA256  $WORK/fonts/Noto-COLRv1.ttf" | sha256sum -c --quiet

gcc "$HERE/webkit-emoji-check.c" -o "$WORK/check" $(pkg-config --cflags --libs webkit2gtk-4.1)

# Carteiro's setup as it writes it at start, plus the COLRv1 font as if the
# system had it installed.
sed -e "s|@EMOJI_DIR@|$ROOT/usr/share/carteiro/fonts|" -e "s|<fontconfig>|<fontconfig><dir>$WORK/fonts</dir>|" \
  "$HERE/../src-tauri/appimage-fonts.conf" > "$WORK/carteiro.conf"
printf '<?xml version="1.0"?>\n<fontconfig><dir>%s</dir><include ignore_missing="yes">/etc/fonts/fonts.conf</include></fontconfig>\n' "$WORK/fonts" > "$WORK/plain.conf"

DISPLAY_WRAP=
if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then DISPLAY_WRAP="xvfb-run -a"; fi
run() {
  # As the AppImage's launcher sets it up; the bundled WebKit finds its
  # helper processes relative to usr/. Off screen, so no compositing.
  (cd "$ROOT/usr" && env FONTCONFIG_FILE="$1" LD_LIBRARY_PATH="$ROOT/usr/lib" WEBKIT_DISABLE_COMPOSITING_MODE=1 \
    GTK_DATA_PREFIX="$ROOT" GTK_EXE_PREFIX="$ROOT/usr" GTK_PATH="$ROOT/usr/lib/gtk-3.0" \
    GIO_MODULE_DIR="$ROOT/usr/lib/gio/modules" GSETTINGS_SCHEMA_DIR="$ROOT/usr/share/glib-2.0/schemas" \
    GDK_PIXBUF_MODULE_FILE="$ROOT/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache" \
    XDG_DATA_DIRS="$ROOT/usr/share:/usr/share" GTK_THEME=Adwaita \
    timeout 60 $DISPLAY_WRAP "$WORK/check" 20 2>"$WORK/stderr" | tail -1) || true
}
# A web process that aborts may be reported late (while its core dump is
# written): its assertion message counts too.
verdict() {
  local out
  out=$(run "$1")
  if grep -qE "Assertion|terminated abnormally" "$WORK/stderr"; then out="web process crashed"; fi
  echo "${out:-no result: $(head -c 300 "$WORK/stderr")}"
}

echo "Without Carteiro's font setup: $(verdict "$WORK/plain.conf")"
result=$(verdict "$WORK/carteiro.conf")
echo "With Carteiro's font setup: $result"
colored=$(sed -n 's/^web process alive, \([0-9]*\) colored pixels$/\1/p' <<<"$result")
[ -n "$colored" ] || { echo "::error::the AppImage's WebKit crashes drawing color emoji"; exit 1; }
[ "$colored" -gt 1000 ] || { echo "::error::emoji aren't drawn in color ($colored colored pixels)"; exit 1; }
