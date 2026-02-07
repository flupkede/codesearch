#!/usr/bin/env bash
# Build script that auto-increments version

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Run version bump
./build.sh

# Build
cargo build "$@"
