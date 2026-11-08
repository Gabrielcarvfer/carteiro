#!/usr/bin/env bash
# Finishes the Android project that `cargo tauri android init` generates
# (src-tauri/gen/android is regenerated, so this runs after every init):
# - adds ClamAV (scripts/build-clamav-android.sh output) for each ABI;
# - installs src-tauri/android/MainActivity.kt (system bar / keyboard insets)
#   and Carteiro's launcher icon;
# - keeps native libraries extracted: Android only executes files from the
#   extracted native library folder;
# - pins the Gradle download by SHA-256 (the generated wrapper doesn't
#   verify it) and gives it a realistic download timeout.
#
# Usage: scripts/prepare-android.sh [clamav-out-dir]   (default target/clamav-android/out)
set -euo pipefail

GRADLE_VERSION=9.6.1
GRADLE_SHA256=9c0f7faeeb306cb14e4279a3e084ca6b596894089a0638e68a07c945a32c9e14

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/src-tauri/gen/android"
OUT="${1:-$ROOT/target/clamav-android/out}"
[ -d "$APP" ] || { echo "run 'cargo tauri android init' first" >&2; exit 1; }

for abi in arm64-v8a x86_64; do
  [ -f "$OUT/$abi/libclamd.so" ] || { echo "ClamAV for $abi missing in $OUT (scripts/build-clamav-android.sh $abi)" >&2; exit 1; }
  mkdir -p "$APP/app/src/main/jniLibs/$abi"
  cp "$OUT/$abi"/lib*.so "$APP/app/src/main/jniLibs/$abi/"
done

# Carteiro's activity: keeps the page clear of the system bars and keyboard.
cp "$ROOT/src-tauri/android/MainActivity.kt" "$APP/app/src/main/java/dev/carteiro/mail/MainActivity.kt"

# Carteiro's launcher icon: the adaptive icon (gradient background + safe-zone
# foreground, src-tauri/android/res) and, from `cargo tauri icon`, the plain
# icons. The generated project otherwise keeps Tauri's default icon.
RES="$APP/app/src/main/res"
[ -f "$ROOT/src-tauri/icons/android/mipmap-mdpi/ic_launcher.png" ] || { echo "run 'cargo tauri icon app-icon.svg' first" >&2; exit 1; }
for d in "$ROOT"/src-tauri/icons/android/mipmap-*dpi; do
  mkdir -p "$RES/$(basename "$d")"
  cp "$d"/ic_launcher.png "$d"/ic_launcher_round.png "$RES/$(basename "$d")/"
done
cp -R "$ROOT/src-tauri/android/res/." "$RES/"

GRADLE="$APP/app/build.gradle.kts"
grep -q useLegacyPackaging "$GRADLE" || printf '\nandroid {\n    packaging {\n        jniLibs {\n            useLegacyPackaging = true\n        }\n    }\n}\n' >> "$GRADLE"

WRAPPER="$APP/gradle/wrapper/gradle-wrapper.properties"
grep -q "gradle-$GRADLE_VERSION-bin.zip" "$WRAPPER" || {
  echo "The generated project wants $(grep distributionUrl "$WRAPPER"); update GRADLE_VERSION/GRADLE_SHA256 in $0" >&2; exit 1; }
sed -i '/^distributionSha256Sum=/d; /^networkTimeout=/d' "$WRAPPER"
printf 'distributionSha256Sum=%s\nnetworkTimeout=120000\n' "$GRADLE_SHA256" >> "$WRAPPER"
echo "Android project ready (ClamAV for arm64-v8a and x86_64, Gradle $GRADLE_VERSION pinned)"
