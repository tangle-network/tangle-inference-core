# tangle-inference-core

Shared engine supervision, family recipes, HTTP payment gates, metrics and QoS
for Tangle inference blueprints. Blueprints retain their adapters, model admission
and pricing rules.

Owner boundaries and adoption requirements are in [MIGRATION.md](MIGRATION.md);
domain terms are in [CONTEXT.md](CONTEXT.md).

Core verifies weight manifests before starting model replicas and their SGLang
router. The SDK owns credentials and GPU facts; unmeasured recipes make no
throughput claim.

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

Auth, credential and release changes require independent review before merge.
