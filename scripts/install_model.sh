#!/usr/bin/env bash
# Install a model from the built-in catalog, then print the serve command.
set -euo pipefail
cd "$(dirname "$0")/.."

MODEL_ID="${1:?usage: install_model.sh <model-id>  (see: pai models list)}"
cargo run -q -p pai-cli -- models install "$MODEL_ID"
cargo run -q -p pai-cli -- models runnable
