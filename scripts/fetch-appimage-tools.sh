#!/usr/bin/env bash
# Pre-seeds the tools `cargo tauri build --bundles appimage` would otherwise
# download unpinned (linuxdeploy, AppRun and the AppImage plugin, the last
# from a moving "continuous" release). Each file is pinned by SHA-256; the
# bundler finds them in its cache and downloads nothing.
#
# Matches tauri-bundler 2.10.4 (tauri-cli 2.12.4): it looks for
# linuxdeploy-07333c6-*. Update this script together with tauri-cli.
set -euo pipefail

DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tauri"
# Verified, untouched copies: the bundler patches its cached linuxdeploy in
# place, so the copies it uses are refreshed from these on every run.
PRISTINE="${XDG_CACHE_HOME:-$HOME/.cache}/carteiro-appimage-tools"
mkdir -p "$DIR" "$PRISTINE"

fetch() { # fetch <name in cache> <url> <sha256>
  local out="$PRISTINE/$1"
  if ! echo "$3  $out" | sha256sum -c - >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --retry 5 --retry-all-errors --retry-delay 10 --connect-timeout 30 -o "$out.part" "$2"
    echo "$3  $out.part" | sha256sum -c - >/dev/null || { echo "checksum mismatch: $2" >&2; rm -f "$out.part"; exit 1; }
    mv "$out.part" "$out"
  fi
  install -m 755 "$out" "$DIR/$1"
}

fetch AppRun-x86_64 \
  https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-x86_64 \
  f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f
fetch linuxdeploy-07333c6-x86_64.AppImage \
  https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy-07333c6/linuxdeploy-x86_64.AppImage \
  36a2d7e274d12e1050d0e9ecfe11d339ed54720b2bec464c286d53f8b07f5c62
# A fixed release instead of "continuous".
fetch linuxdeploy-plugin-appimage.AppImage \
  https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/1-alpha-20250213-1/linuxdeploy-plugin-appimage-x86_64.AppImage \
  992d502a248e14ab185448ddf6f6e7d25558cb84d4623c354c3af350c25fccb3
fetch runtime-x86_64 \
  https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64 \
  2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
chmod 644 "$DIR/runtime-x86_64"  # data for appimagetool, not a program to run

if [ -n "${GITHUB_ENV:-}" ]; then
  echo "LDAI_RUNTIME_FILE=$DIR/runtime-x86_64" >> "$GITHUB_ENV"
fi
echo "AppImage tools ready in $DIR"
echo "export LDAI_RUNTIME_FILE=$DIR/runtime-x86_64"
