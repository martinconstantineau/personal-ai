#!/usr/bin/env bash
# Build the Rust core shared library for the Flutter app.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
if [ "$PROFILE" = "release" ]; then
  cargo build -p pai-ffi --release
else
  cargo build -p pai-ffi
fi

# Where the Flutter linux bundle looks for it (see linux/CMakeLists.txt).
LIB="$(pwd)/target/${PROFILE}/libpai_ffi.so"
[ -f "$LIB" ] && echo "Built $LIB"
