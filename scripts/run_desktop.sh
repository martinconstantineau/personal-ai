#!/usr/bin/env bash
# Build the core and run the Flutter desktop shell.
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/build_core.sh "${PAI_PROFILE:-debug}"
cd apps/desktop
flutter run -d linux "${@}"
