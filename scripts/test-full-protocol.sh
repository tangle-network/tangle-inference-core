#!/usr/bin/env bash
# Real core chain integration, optionally followed by a blueprint's public checks.
set -euo pipefail
CORE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v anvil >/dev/null
ANVIL_E2E=1 cargo test --locked --all-features --manifest-path "$CORE_DIR/Cargo.toml"
node "$CORE_DIR/tests/sign_spend_auth.mjs"
if [ "$#" -gt 0 ]; then
    BLUEPRINT_DIR="$(cd "$1" && pwd)"
    cargo test --locked --manifest-path "$BLUEPRINT_DIR/Cargo.toml"
    (cd "$BLUEPRINT_DIR/contracts" && forge test --remappings "tangle-inference-core/=$CORE_DIR/contracts/")
fi
