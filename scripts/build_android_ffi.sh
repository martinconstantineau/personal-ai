#!/usr/bin/env bash
# Build the Rust core shared library for Android ABIs into the Flutter
# project's jniLibs dir. Requires:
#   - rustup targets: aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   - cargo-ndk (cargo install cargo-ndk)
#   - Android NDK (set ANDROID_NDK_HOME, or let cargo-ndk discover it)
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
