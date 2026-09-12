#!/usr/bin/env bash
# The gate that must be green before every merge.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

if command -v flutter >/dev/null && [ -d apps/desktop ]; then
  (cd apps/desktop && flutter pub get >/dev/null && flutter analyze && flutter test)
fi

echo "All checks passed."
