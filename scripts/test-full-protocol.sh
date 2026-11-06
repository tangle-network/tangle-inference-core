#!/usr/bin/env bash
# Real core chain integration.
set -euo pipefail
CORE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v anvil >/dev/null
ANVIL_E2E=1 cargo test --locked --all-features --manifest-path "$CORE_DIR/Cargo.toml"
