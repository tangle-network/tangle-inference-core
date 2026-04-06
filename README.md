# tangle-inference-core

Shared infrastructure for building Tangle inference operator blueprints. Handles billing, x402 payment validation, nonce replay protection, Prometheus metrics, and GPU health — so your blueprint only needs to implement inference.

```toml
[dependencies]
tangle-inference-core = { git = "https://github.com/tangle-network/tangle-inference-core", branch = "master" }
```

## Quick Start

```rust
use std::sync::Arc;
use tangle_inference_core::{
    AppState, AppStateBuilder, BillingClient, NonceStore,
    BillingConfig, ServerConfig, TangleConfig,
};

// Your inference backend — any Send + Sync + 'static type
struct MyBackend { /* vLLM handle, Ollama client, etc. */ }

// Build the shared operator state
let state: AppState = AppStateBuilder::new()
    .billing(Arc::new(BillingClient::new(&tangle_config, &billing_config)?))
    .nonce_store(Arc::new(NonceStore::load(billing_config.nonce_store_path.clone())))
    .server_config(Arc::new(server_config))
    .billing_config(Arc::new(billing_config))
    .tangle_config(Arc::new(tangle_config))
    .operator_address(operator_address)
    .backend(MyBackend { /* ... */ })
    .build()?;

// In your axum handler — retrieve backend + validate payments
async fn chat(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<Request>) -> Response {
    let backend = state.backend::<MyBackend>().unwrap();

    // Extract and validate x402 SpendAuth
    let spend_auth = extract_x402_spend_auth(&headers).unwrap();
    let preauth = validate_spend_auth(&state, &spend_auth).await?;

    // Run inference
    let result = backend.generate(&req).await;

    // Settle on-chain (authorizeSpend + claimPayment)
    let cost = my_cost_model.calculate_cost(&CostParams {
        prompt_tokens: result.prompt_tokens,
        completion_tokens: result.completion_tokens,
        ..Default::default()
    });
    settle_billing(&state.billing, &spend_auth, preauth, cost).await;

    Json(result).into_response()
}
```

## Modules

| Module | What it does |
|--------|-------------|
| **`billing`** | `BillingClient` — submits `authorizeSpend` and `claimPayment` txs to ShieldedCredits via alloy. EIP-712 signature recovery. Gas price cap. Retry with exponential backoff. |
| **`server`** | `AppState` + builder, `NonceStore` (file-backed replay protection), `validate_spend_auth`, `settle_billing`, `extract_x402_spend_auth`, `payment_required`, `error_response` |
| **`metrics`** | Global Prometheus registry. `RequestGuard` RAII — tracks latency, tokens, TTFT, active requests. `on_chain_metrics()` for QoS heartbeat. |
| **`health`** | `detect_gpus()` — parses `nvidia-smi`, returns `Vec<GpuInfo>` |
| **`config`** | `TangleConfig`, `BillingConfig`, `ServerConfig`, `GpuConfig` with serde + sensible defaults |

## Cost Models

Implement `CostModel` or use one of the built-in implementations:

```rust
pub trait CostModel: Send + Sync + 'static {
    fn calculate_cost(&self, params: &CostParams) -> u64;
}
```

| Model | Use case | Pricing input |
|-------|----------|---------------|
| `PerTokenCostModel` | LLM chat/completion | input + output tokens |
| `PerCharCostModel` | Text-to-speech | characters |
| `PerSecondCostModel` | Speech-to-text, video | centiseconds |
| `PerImageCostModel` | Image generation | image count |
| `FlatRequestCostModel` | Embeddings | flat fee per request |
| `TaskTypeCostModel` | Multi-task (Modal) | dispatches by task type |

## SpendAuth Validation

`validate_spend_auth` performs the full pre-flight check in order:

1. Parse amount
2. Enforce min charge + max spend policy
3. Verify operator address matches this operator
4. Verify service ID (if configured)
5. Nonce replay check + record (prevents double-spend)
6. EIP-712 signature recovery (alloy k256)
7. On-chain spending key verification
8. Minimum balance check

Returns `Ok(preauth_amount)` or a ready-to-return HTTP error response.

## SpendAuth Serde

`SpendAuthPayload` accepts both camelCase and snake_case, and numeric fields accept JSON numbers or strings (JS BigInts serialize as strings):

```json
{"commitment":"0x...","serviceId":"1","jobIndex":0,"amount":"1000000","operator":"0x...","nonce":"0","expiry":"1775500000","signature":"0x..."}
```

## Backend

The backend is type-erased (`Arc<dyn Any + Send + Sync>`). Attach any type via the builder, retrieve it in handlers with zero-cost downcast:

```rust
// Attach
.backend(VllmProcess::spawn().await?)

// Retrieve
let vllm: &VllmProcess = state.backend::<VllmProcess>().unwrap();
```

## Blueprints Using This Crate

- [llm-inference-blueprint](https://github.com/tangle-network/llm-inference-blueprint) (vLLM / Ollama)
- [voice-inference-blueprint](https://github.com/tangle-network/voice-inference-blueprint) (TTS/STT)
- [image-gen-inference-blueprint](https://github.com/tangle-network/image-gen-inference-blueprint)
- [embedding-inference-blueprint](https://github.com/tangle-network/embedding-inference-blueprint)
- [video-gen-inference-blueprint](https://github.com/tangle-network/video-gen-inference-blueprint)
- [modal-inference-blueprint](https://github.com/tangle-network/modal-inference-blueprint)
- [distributed-inference-blueprint](https://github.com/tangle-network/distributed-inference-blueprint)

## Testing

```bash
cargo test
```

Integration tests construct a real `AppState`, exercise every `CostModel`, sign a `SpendAuthPayload` with a known keypair, round-trip it through EIP-712 recovery, and verify `NonceStore` persistence across reloads.

## License

Apache-2.0
