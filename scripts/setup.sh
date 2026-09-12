#!/usr/bin/env bash
# Dev environment setup (Debian/Ubuntu). Idempotent.
set -euo pipefail

if command -v apt-get >/dev/null; then
  sudo apt-get update -qq
  sudo apt-get install -y --no-install-recommends \
    build-essential clang cmake ninja-build pkg-config \
    libgtk-3-dev liblzma-dev libstdc++-12-dev libglvnd-dev \
    curl git unzip xz-utils
fi

if ! command -v rustup >/dev/null && ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi
rustup component add clippy rustfmt 2>/dev/null || true

if ! command -v flutter >/dev/null; then
  echo "Flutter not on PATH. Install stable from https://docs.flutter.dev/get-started/install"
  echo "then re-run this script."
else
  flutter config --enable-linux-desktop >/dev/null
  flutter --version
fi

echo "Setup complete. Run ./scripts/test.sh to verify."
