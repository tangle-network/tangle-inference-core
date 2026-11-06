# tangle-inference-core

Run inference operators with shared SGLang/vLLM supervision, model-family recipes,
HTTP payment gates, metered settlement, replay protection, metrics and QoS.
Blueprints supply their request adapters, prices and model admission rules.

Pin this crate by full Git commit. [MIGRATION.md](MIGRATION.md) describes the
consumer imports, shared runner and deployment scripts.

## Serve a model family

```rust,no_run
use tangle_inference_core::engine::{EngineConfig, EngineKind, EngineProcess};

# async fn example() -> anyhow::Result<()> {
let engine = EngineProcess::spawn(EngineConfig {
    engine: EngineKind::Sglang,
    base_family: Some("qwen3".into()),
    model: "Qwen/Qwen3-0.6B".into(),
    tensor_parallel_size: 1,
    ..EngineConfig::default()
}).await?;
engine.wait_ready().await?;
// Run supervise with the operator's shutdown receiver while serving requests.
engine.shutdown().await;
# Ok(())
# }
```

[engine-profiles.json](engine-profiles.json) owns the pinned SGLang image and
family flags. Its initial import preserves abliterated-inference's catalog bytes.
A derivative sets `base_family` and changes `model`; a new family adds a catalog
entry. `RecipeCatalog::resolve` returns the image and argv for lease preparation.
Install the engine in that image; core supervises its process on the node.
Blueprint-specific chain admission and price configuration remain separate.

`EngineProcess::connect` attaches an external server, including the existing
operator-lite/Ollama path. Managed engines run in their own process group so
shutdown and restart also terminate workers.

## Register and bill

The shared [registration script](contracts/script/RegisterBlueprint.s.sol) reads
`metadata.profiling_data` from the blueprint definition and sends it unchanged to
Tangle. Each blueprint provides its manager deployment adapter.

`AppStateBuilder`, `billing_gate` and `settle_billing` share authorization and
settlement. Pass the metered cost as `Some(cost)`; `None` claims the full preauth.
Keep nonce and direct-transfer replay stores persistent across operator restarts.
`qos::start_heartbeat` submits the same core metrics used by the HTTP endpoints.

Core currently retains its NVIDIA detection helper and legacy GPU configuration.
The D9 lane STATUS tracks replacing these with the SDK's detection, catalog and
class/VRAM-per-GPU API when released.

## Check changes

Run the repository's admitted Beelink gate against the exact candidate commit:

```bash
beelink-gate auto git@github.com:tangle-network/tangle-inference-core.git "$HEAD" -- bash -c 'cargo fmt --all -- --check && cargo check --locked --no-default-features && cargo test --locked --all-features && cargo clippy --locked --all-features --all-targets -- -D warnings && bash scripts/test-full-protocol.sh'
```

The protocol check requires Foundry and runs real Anvil payment integration.
Engine tests use child processes and HTTP. Shared signing takes its key on stdin.
Core changes also require the downstream operator-lite cached-token billing proof
before merge. Consumers pin the qualified default-branch commit; this repository
has no package-publishing workflow or hosted PR gate.
