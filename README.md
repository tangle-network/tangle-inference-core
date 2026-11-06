# tangle-inference-core

Shared engine supervision, family recipes, HTTP payment gates, metrics and QoS
for Tangle inference blueprints. Blueprints retain their adapters, model admission
and pricing rules.

The corrected serving program is being implemented on this branch. Its owner
boundaries and adoption requirements are in [MIGRATION.md](MIGRATION.md); domain
terms are in [CONTEXT.md](CONTEXT.md).

Core owns the model-serving process and recipe resolver. The corrected pipeline
stages a complete SHA-256 manifest, starts a per-model replica set and its
SGLang router, and exposes readiness through the operator adapter. Child
processes receive an explicit environment allowlist; node credentials come from
the SDK keystore. The SDK owns GPU class identifiers, decimal VRAM facts and TEE
detection. Measurement rows identify the actual model, engine, workload and
provider SKU; unmeasured recipes make no throughput claim.

Registration and signing remain with their native owners: cargo-tangle registers,
cargo-dist releases, tnt-core initializes blueprint managers, and tcloud signs
client SpendAuth messages. Core does not duplicate those implementations.

Check a candidate with the repository's admitted Beelink gate. The protocol
script runs real Anvil payment flows; engine and staging tests cross public APIs,
child processes and HTTP. Changed serving inputs also require the maintained
operator-lite cached-billing proof and D11 parity before release.

```bash
beelink-gate --merge "$PR" auto git@github.com:tangle-network/tangle-inference-core.git "$HEAD" -- env RUSTUP_TOOLCHAIN=1.93 bash -c 'cargo fmt --all -- --check && cargo check --locked --no-default-features && cargo clippy --locked --all-features --all-targets -- -D warnings && bash scripts/test-full-protocol.sh'
```

Auth, credential and release changes require a hold label and independent review
before merge. Classifier-stopped work waits for Drew; no alternate reviewer runs
that work. The lane STATUS records measured results and dependency blockers.
