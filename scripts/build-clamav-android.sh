#!/usr/bin/env bash
# Cross-compiles ClamAV's clamd and freshclam for Android, statically linked
# against pinned, checksum-verified dependencies, and installs them where the
# APK packs native libraries (Android only lets apps execute files from their
# native library directory, hence the lib*.so names):
#
#   src-tauri/gen/android/app/src/main/jniLibs/<abi>/libclamd.so
#   src-tauri/gen/android/app/src/main/jniLibs/<abi>/libfreshclam.so
#
# Usage: NDK_HOME=/path/to/ndk scripts/build-clamav-android.sh arm64-v8a|x86_64
# Needs: cmake, make, a host C compiler, curl, and a Rust toolchain with the
# matching Android target (aarch64-linux-android / x86_64-linux-android).
# ClamAV is GPL-2.0 and runs as a separate program; see THIRD-PARTY.md.
set -euo pipefail

ABI="${1:?usage: $0 arm64-v8a|x86_64}"
API=28 # bionic provides iconv from API 28 (Android 9)
case "$ABI" in
  arm64-v8a) TRIPLE=aarch64-linux-android; OSSL_TARGET=android-arm64 ;;
  x86_64)    TRIPLE=x86_64-linux-android;  OSSL_TARGET=android-x86_64 ;;
  *) echo "unsupported ABI: $ABI" >&2; exit 2 ;;
esac
: "${NDK_HOME:?set NDK_HOME to the Android NDK}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/target/clamav-android/src"
WORK="$ROOT/target/clamav-android/$ABI"
PREFIX="$WORK/prefix"
JNI="${JNILIBS_DIR:-$ROOT/src-tauri/gen/android/app/src/main/jniLibs}/$ABI"
JOBS="$(nproc 2>/dev/null || echo 4)"
mkdir -p "$SRC" "$WORK" "$PREFIX/lib" "$PREFIX/include" "$JNI"
# Work in the build folder: tools drop files in the current directory (e.g.
# rustc writes libnull.a when ClamAV probes it).
cd "$WORK"

TC="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64"
export PATH="$TC/bin:$PATH"
export CC="$TC/bin/${TRIPLE}${API}-clang" CXX="$TC/bin/${TRIPLE}${API}-clang++"
export AR="$TC/bin/llvm-ar" RANLIB="$TC/bin/llvm-ranlib" STRIP="$TC/bin/llvm-strip"
export CFLAGS="-O2 -fPIC" ANDROID_NDK_ROOT="$NDK_HOME"
# libclamav's Rust part links with the NDK's clang.
TRIPLE_ENV="$(echo "$TRIPLE" | tr 'a-z-' 'A-Z_')"
export "CARGO_TARGET_${TRIPLE_ENV}_LINKER=$CC"

CMAKE_ANDROID=(
  -DCMAKE_TOOLCHAIN_FILE="$NDK_HOME/build/cmake/android.toolchain.cmake"
  -DANDROID_ABI="$ABI" -DANDROID_PLATFORM="android-$API"
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX"
  -DCMAKE_PREFIX_PATH="$PREFIX" -DCMAKE_FIND_ROOT_PATH="$PREFIX"
  -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON
)

# fetch <file> <url> <sha256>: download once, verify, unpack into $WORK.
fetch() {
  local file="$1" url="$2" sha="$3"
  [ -f "$SRC/$file" ] || curl -fsSL --proto '=https' --retry 5 --retry-all-errors --retry-delay 10 --connect-timeout 30 -o "$SRC/$file" "$url"
  echo "$sha  $SRC/$file" | sha256sum -c - >/dev/null || { echo "checksum mismatch: $file" >&2; rm -f "$SRC/$file"; exit 1; }
  tar -xf "$SRC/$file" -C "$WORK"
}

cmake_dep() { # cmake_dep <dir> [extra cmake args...]
  local dir="$WORK/$1"; shift
  cmake -S "$dir" -B "$dir/build" -G "Unix Makefiles" "${CMAKE_ANDROID[@]}" "$@" >/dev/null
  cmake --build "$dir/build" -j "$JOBS" >/dev/null
  cmake --install "$dir/build" >/dev/null
}

# Each dependency is built once per ABI (marker files in $PREFIX).
built() { [ -f "$PREFIX/.built-$1" ]; }
mark() { touch "$PREFIX/.built-$1"; }

echo "== zlib"
if ! built zlib; then
fetch zlib-1.3.1.tar.gz https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz \
  9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23
(cd "$WORK/zlib-1.3.1" && ./configure --static --prefix="$PREFIX" >/dev/null && make -j "$JOBS" >/dev/null && make install >/dev/null)
mark zlib
fi

echo "== bzip2"
if ! built bzip2; then
fetch bzip2-1.0.8.tar.gz https://sourceware.org/pub/bzip2/bzip2-1.0.8.tar.gz \
  ab5a03176ee106d3f0fa90e381da478ddae405918153cca248e682cd0c4a2269
(cd "$WORK/bzip2-1.0.8" && make -j "$JOBS" CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="$CFLAGS -D_FILE_OFFSET_BITS=64" libbz2.a >/dev/null \
  && cp libbz2.a "$PREFIX/lib/" && cp bzlib.h "$PREFIX/include/")
mark bzip2
fi

echo "== OpenSSL"
if ! built openssl; then
fetch openssl-3.5.4.tar.gz https://github.com/openssl/openssl/releases/download/openssl-3.5.4/openssl-3.5.4.tar.gz \
  967311f84955316969bdb1d8d4b983718ef42338639c621ec4c34fddef355e99
# The API level comes from $CC (…android28-clang); passing -D__ANDROID_API__
# too only redefines it (a warning in every file).
(cd "$WORK/openssl-3.5.4" && ./Configure "$OSSL_TARGET" no-shared no-tests no-docs no-apps \
  --prefix="$PREFIX" --libdir=lib >/dev/null && make -j "$JOBS" >/dev/null && make install_sw >/dev/null)
mark openssl
fi

echo "== PCRE2"
if ! built pcre2; then
fetch pcre2-10.45.tar.gz https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.45/pcre2-10.45.tar.gz \
  0e138387df7835d7403b8351e2226c1377da804e0737db0e071b48f07c9d12ee
cmake_dep pcre2-10.45 -DPCRE2_BUILD_PCRE2GREP=OFF -DPCRE2_BUILD_TESTS=OFF -DPCRE2_SUPPORT_JIT=OFF
mark pcre2
fi

echo "== libxml2"
if ! built libxml2; then
fetch libxml2-2.14.6.tar.xz https://download.gnome.org/sources/libxml2/2.14/libxml2-2.14.6.tar.xz \
  7ce458a0affeb83f0b55f1f4f9e0e55735dbfc1a9de124ee86fb4a66b597203a
cmake_dep libxml2-2.14.6 -DLIBXML2_WITH_PYTHON=OFF -DLIBXML2_WITH_ICONV=OFF -DLIBXML2_WITH_LZMA=OFF -DLIBXML2_WITH_ZLIB=OFF \
  -DLIBXML2_WITH_ICU=OFF -DLIBXML2_WITH_TESTS=OFF -DLIBXML2_WITH_PROGRAMS=OFF
mark libxml2
fi

echo "== json-c"
if ! built jsonc; then
fetch json-c-0.18.tar.gz https://s3.amazonaws.com/json-c_releases/releases/json-c-0.18.tar.gz \
  876ab046479166b869afc6896d288183bbc0e5843f141200c677b3e8dfb11724
cmake_dep json-c-0.18 -DBUILD_TESTING=OFF -DBUILD_STATIC_LIBS=ON -DDISABLE_EXTRA_LIBS=ON -DBUILD_APPS=OFF
mark jsonc
fi

echo "== curl"
if ! built curl; then
fetch curl-8.16.0.tar.xz https://curl.se/download/curl-8.16.0.tar.xz \
  40c8cddbcb6cc6251c03dea423a472a6cea4037be654ba5cf5dec6eb2d22ff1d
# freshclam verifies database.clamav.net against Android's system CAs.
cmake_dep curl-8.16.0 -DCURL_USE_OPENSSL=ON -DOPENSSL_ROOT_DIR="$PREFIX" -DOPENSSL_USE_STATIC_LIBS=ON \
  -DCURL_CA_PATH=/system/etc/security/cacerts -DCURL_CA_BUNDLE=none -DCURL_DISABLE_LDAP=ON -DCURL_USE_LIBPSL=OFF \
  -DUSE_LIBIDN2=OFF -DCURL_USE_LIBSSH2=OFF -DCURL_BROTLI=OFF -DCURL_ZSTD=OFF -DUSE_NGHTTP2=OFF -DBUILD_CURL_EXE=OFF \
  -DENABLE_CURL_MANUAL=OFF -DBUILD_TESTING=OFF -DBUILD_LIBCURL_DOCS=OFF -DBUILD_MISC_DOCS=OFF
mark curl
fi

echo "== ncurses (required by ClamAV's app build)"
if ! built ncurses; then
fetch ncurses-6.5.tar.gz https://ftp.gnu.org/gnu/ncurses/ncurses-6.5.tar.gz \
  136d91bc269a9a5785e5f9e980bc76ab57428f604ce3e5a5a90cebc767971cc6
(cd "$WORK/ncurses-6.5" && ./configure --host="$TRIPLE" --prefix="$PREFIX" --without-shared --without-cxx-binding --without-ada \
  --without-progs --without-tests --without-manpages --disable-stripping --with-build-cc="${BUILD_CC:-cc}" >/dev/null \
  && make -j "$JOBS" >/dev/null && make install >/dev/null)
mark ncurses
fi

echo "== ClamAV"
fetch clamav-1.5.4.tar.gz https://github.com/Cisco-Talos/clamav/releases/download/clamav-1.5.4/clamav-1.5.4.tar.gz \
  1af1117a228f1b5bc7fa91a0dabc37848a99e7d25188e9be8043332ce721dfd3
CL="$WORK/clamav-1.5.4"
# Always configure ClamAV afresh: a build left over from another Rust
# toolchain fails with "compiled by an incompatible version of rustc".
rm -rf "$CL/build"
# ClamAV asks rustc which system libraries Rust needs without naming the
# target, so it gets the host's (-lgcc_s -lutil -lrt ...), which Android
# lacks. Ask for the Android target's list instead.
sed -i 's|${rustc_EXECUTABLE} --crate-type staticlib --print=native-static-libs|${rustc_EXECUTABLE} --target ${RUST_COMPILER_TARGET} --crate-type staticlib --print=native-static-libs|' "$CL/cmake/FindRust.cmake"
grep -q -- '--target ${RUST_COMPILER_TARGET} --crate-type staticlib' "$CL/cmake/FindRust.cmake" || { echo "FindRust.cmake patch failed" >&2; exit 1; }
# freshclam links -lresolv; on Android the resolver (res_query etc.) is part
# of libc, so an empty libresolv.a satisfies the name.
mkdir -p "$PREFIX/stubs" && "$AR" rcs "$PREFIX/stubs/libresolv.a"
cmake -S "$CL" -B "$CL/build" -G "Unix Makefiles" "${CMAKE_ANDROID[@]}" \
  -DRUST_COMPILER_TARGET="$TRIPLE" \
  -DENABLE_SHARED_LIB=OFF -DENABLE_STATIC_LIB=ON -DENABLE_APP=ON \
  -DENABLE_MILTER=OFF -DENABLE_CLAMONACC=OFF -DENABLE_SYSTEMD=OFF -DENABLE_TESTS=OFF \
  -DENABLE_MAN_PAGES=OFF -DENABLE_DOXYGEN=OFF -DENABLE_EXAMPLES=OFF -DENABLE_JSON_SHARED=OFF \
  -DBYTECODE_RUNTIME=interpreter -DOPENSSL_ROOT_DIR="$PREFIX" -DOPENSSL_USE_STATIC_LIBS=ON \
  `# Run-time feature checks can't execute when cross-compiling. They test FD` \
  `# passing over sockets, packed structs and mallinfo; all hold on Android.` \
  -Dtest_run_result=0 -Dtest_run_result__TRYRUN_OUTPUT= \
  -DNCURSES_INCLUDE_DIR="$PREFIX/include/ncursesw" -DCURSES_LIBRARY="$PREFIX/lib/libncursesw.a" \
  -DCMAKE_EXE_LINKER_FLAGS="-L$PREFIX/stubs"
cmake --build "$CL/build" -j "$JOBS" --target clamd freshclam-bin

install -m 755 "$(find "$CL/build" -type f -name clamd -perm -u+x | head -1)" "$JNI/libclamd.so"
install -m 755 "$(find "$CL/build" -type f -name freshclam -perm -u+x | head -1)" "$JNI/libfreshclam.so"
"$STRIP" "$JNI/libclamd.so" "$JNI/libfreshclam.so"
echo "Installed ClamAV for $ABI into $JNI"
ls -la "$JNI"
