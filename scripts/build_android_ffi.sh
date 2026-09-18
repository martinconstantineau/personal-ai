#!/usr/bin/env bash
# Build the Rust core shared library for Android ABIs into the Flutter
# project's jniLibs dir. Requires:
#   - rustup targets: aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   - cargo-ndk (cargo install cargo-ndk)
#   - Android NDK (ANDROID_NDK_HOME; auto-derived from ANDROID_SDK_ROOT/ndk
#     when unset)
#   - Prebuilt OpenSSL static libs per ABI under OPENSSL_OUT (vendored
#     openssl-src cannot cross-compile on Windows hosts). Build them from
#     an OpenSSL source tree in an MSYS2 shell:
#       ./Configure android-arm64 -D__ANDROID_API__=26 no-shared no-tests \
#           --prefix=$PWD/out && make -j build_libs && make install_dev
#     (targets: android-arm64 / android-arm / android-x86_64)
#
# Usage: OPENSSL_OUT=~/src/ossl-out scripts/build_android_ffi.sh [debug]
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-release}"
OSSL="${OPENSSL_OUT:?set OPENSSL_OUT to the dir containing arm64/ armv7/ x86_64/ openssl installs}"

# cargo-ndk needs ANDROID_NDK_HOME; derive it from the SDK's newest
# installed NDK when unset so ANDROID_SDK_ROOT alone is enough.
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  for sdk in "${ANDROID_SDK_ROOT:-}" "${ANDROID_HOME:-}"; do
    if [ -n "$sdk" ] && [ -d "$sdk/ndk" ]; then
      ANDROID_NDK_HOME="$(ls -d "$sdk"/ndk/*/ 2>/dev/null | sort -V | tail -1)"
      [ -n "$ANDROID_NDK_HOME" ] && export ANDROID_NDK_HOME && break
    fi
  done
  if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    echo "error: no NDK found — set ANDROID_NDK_HOME or ANDROID_SDK_ROOT" >&2
    exit 1
  fi
fi

# AAudio (cpal) needs API 26+. Keep in sync with minSdk in
# apps/desktop/android/app/build.gradle.kts.
PLATFORM=26
OUT=apps/desktop/android/app/src/main/jniLibs

export OPENSSL_NO_VENDOR=1 OPENSSL_STATIC=1
export AARCH64_LINUX_ANDROID_OPENSSL_LIB_DIR="$OSSL/arm64/lib"
export AARCH64_LINUX_ANDROID_OPENSSL_INCLUDE_DIR="$OSSL/arm64/include"
export ARMV7_LINUX_ANDROIDEABI_OPENSSL_LIB_DIR="$OSSL/armv7/lib"
export ARMV7_LINUX_ANDROIDEABI_OPENSSL_INCLUDE_DIR="$OSSL/armv7/include"
export X86_64_LINUX_ANDROID_OPENSSL_LIB_DIR="$OSSL/x86_64/lib"
export X86_64_LINUX_ANDROID_OPENSSL_INCLUDE_DIR="$OSSL/x86_64/include"

if [ "$PROFILE" = "release" ]; then
  cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 --platform "$PLATFORM" \
    -o "$OUT" build -p pai-ffi --release
else
  cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 --platform "$PLATFORM" \
    -o "$OUT" build -p pai-ffi
fi

find "$OUT" -name 'libpai_ffi.so'
