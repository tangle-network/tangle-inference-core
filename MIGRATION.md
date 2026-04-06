# Migration Path: Inference Blueprint → `tangle-inference-core`

This document is the canonical recipe for migrating an inference blueprint to depend on `tangle-inference-core`. It's based on the `llm-inference-blueprint` migration, which serves as the reference implementation.

## Reference Migration

The canonical reference for this migration is [`llm-inference-blueprint`](../llm-inference-blueprint/) (formerly `vllm-inference-blueprint`). Study it before migrating.

**Before migration:**
- `billing.rs` (428 LOC), `metrics.rs` (508 LOC), `health.rs` (135 LOC) — full copies of shared infrastructure
- `server.rs` (1070 LOC) — Axum handlers, nonce store, SpendAuth validation, x402 headers
- `config.rs` (520 LOC) — BillingConfig, ServerConfig, GpuConfig, TangleConfig all inlined
- **Total: ~3,640 LOC**

**After migration:**
- Those three files **deleted**
- `server.rs` shrunk to 631 LOC (−439)
- `config.rs` shrunk to 221 LOC (−299)
- **Total: ~1,819 LOC, a 50% reduction**

**Tests:** 5 lib unit tests + 26 server integration tests passing. Clippy clean.

## The Pattern

Every inference blueprint shares the same operator-side infrastructure:
- ShieldedCredits billing (authorizeSpend / claimPayment)
- EIP-712 SpendAuth verification
- x402 HTTP payment headers + 402 Payment Required responses
- Nonce replay protection
- Prometheus metrics registry + `RequestGuard` RAII
- GPU detection via nvidia-smi
- Config loading (TOML + env var overrides)

Blueprints differ only in:
- The **backend** they call (vLLM subprocess, HTTP proxy to Modal, ComfyUI, TEI, etc.)
- The **cost model** (per-token, per-char, per-second, per-image, task-type)
- The **job schema** (what's in the request/response)
- The **HTTP endpoints** they expose

This migration moves the shared parts to `tangle-inference-core` and keeps only the blueprint-specific parts in the blueprint repo.

## Step-by-Step Recipe

### Step 1: Add the dependency

```toml
# operator/Cargo.toml
[dependencies]
tangle-inference-core = { path = "../../tangle-inference-core" }
```

### Step 2: Identify the four categories

Walk through each file in `operator/src/` and classify:

| Category | Action | Examples |
|---|---|---|
| **Shared infrastructure** | Delete the file, import from core | `billing.rs`, `metrics.rs`, `health.rs` |
| **Shared config** | Delete those config structs, re-use core's | `BillingConfig`, `ServerConfig`, `GpuConfig`, `TangleConfig` |
| **Backend glue** | Keep — this is the unique value of your blueprint | `vllm.rs`, `modal_proxy.rs`, `voice_engine.rs`, `diffusion.rs`, `video.rs`, `embedding.rs`, `pipeline.rs` |
| **Blueprint-specific logic** | Keep — routes, handlers, job schemas | `server.rs`'s HTTP handlers and Axum routes, `lib.rs`'s job handler for on-chain calls |

### Step 3: Delete the shared files

```bash
rm operator/src/billing.rs
rm operator/src/metrics.rs
rm operator/src/health.rs
```

If your blueprint didn't have `billing.rs` (embedding, image-gen, video-gen, distributed), you're adding billing *via* core instead of refactoring it out. Same steps apply.

### Step 4: Rewrite `config.rs`

Before (typical):
```rust
pub struct OperatorConfig {
    pub tangle: TangleConfig,
    pub server: ServerConfig,
    pub billing: BillingConfig,
    pub gpu: GpuConfig,
    pub vllm: VllmConfig,
    // ... dozens of fields inlined
}

#[derive(Deserialize)]
pub struct BillingConfig {
    pub price_per_input_token: u64,
    pub price_per_output_token: u64,
    pub max_gas_price_gwei: u64,
    // ... many fields
}

#[derive(Deserialize)]
pub struct ServerConfig { /* ... */ }
#[derive(Deserialize)]
pub struct GpuConfig { /* ... */ }
#[derive(Deserialize)]
pub struct TangleConfig { /* ... */ }
```

After:
```rust
use tangle_inference_core::{BillingConfig, GpuConfig, ServerConfig, TangleConfig};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OperatorConfig {
    pub tangle: TangleConfig,
    pub server: ServerConfig,
    pub billing: BillingConfig,
    pub gpu: GpuConfig,
    // Your backend-specific config section (the only thing you define)
    pub vllm: VllmConfig,  // or modal, voice, embedding, etc.
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VllmConfig {
    pub model: String,
    pub host: String,
    pub port: u16,
    // Pricing lives with the backend now (the blueprint's unique value)
    pub price_per_input_token: u64,
    pub price_per_output_token: u64,
    // ... blueprint-specific fields
}
```

**Wire-format note:** this is a breaking config change. If you had `billing.price_per_input_token` in your existing deployed configs, update them to `<backend>.price_per_input_token`.

### Step 5: Define a `Backend` struct

Each blueprint defines its own backend struct holding runtime state plus a `CostModel`. This gets attached to the shared `AppState` via `AppStateBuilder`.

```rust
// operator/src/server.rs
use std::sync::Arc;
use tangle_inference_core::{
    AppState, AppStateBuilder, BillingClient, NonceStore,
    PerTokenCostModel, CostModel, CostParams,
};

pub struct VllmBackend {
    pub process: Arc<VllmProcess>,
    pub config: Arc<OperatorConfig>,
    pub cost_model: Arc<PerTokenCostModel>,
}

impl VllmBackend {
    pub fn new(config: Arc<OperatorConfig>, process: Arc<VllmProcess>) -> Self {
        Self {
            cost_model: Arc::new(PerTokenCostModel {
                price_per_input_token: config.vllm.price_per_input_token,
                price_per_output_token: config.vllm.price_per_output_token,
            }),
            process,
            config,
        }
    }
}
```

### Step 6: Build `AppState` via `AppStateBuilder`

```rust
// In your server startup code
use tangle_inference_core::AppStateBuilder;

pub async fn start_server(config: Arc<OperatorConfig>, process: Arc<VllmProcess>) -> Result<()> {
    let billing_client = BillingClient::new(&config.tangle, &config.billing).await?;
    let nonce_store = Arc::new(NonceStore::load(config.billing.nonce_store_path.clone()));
    let operator_addr = billing_client.operator_address();

    let state = AppStateBuilder::new()
        .billing(Arc::new(billing_client))
        .nonce_store(nonce_store)
        .server_config(Arc::new(config.server.clone()))
        .billing_config(Arc::new(config.billing.clone()))
        .operator_address(operator_addr)
        .max_concurrent(config.server.max_concurrent_requests)
        .backend(VllmBackend::new(config, process))
        .build()?;

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        // ... your routes
        .with_state(state);

    // ... serve
}
```

### Step 7: Retrieve backend in handlers

```rust
use axum::extract::State;
use tangle_inference_core::{
    AppState, validate_spend_auth, extract_x402_spend_auth,
    payment_required, error_response, RequestGuard,
};

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    let backend = state
        .backend::<VllmBackend>()
        .expect("VllmBackend attached to AppState");

    // 1. Extract SpendAuth from x402 headers (shared helper)
    let spend_auth = match extract_x402_spend_auth(&headers) {
        Some(a) => a,
        None => return payment_required(
            &state.billing_config,
            &backend.config.tangle,
            state.operator_address,
            /*estimated=*/ 1000,
        ),
    };

    // 2. Validate via shared helper (nonce, balance, signature, expiry)
    if let Err(e) = validate_spend_auth(&state, &spend_auth).await {
        return error_response(e);
    }

    // 3. Record metrics via shared RequestGuard
    let mut guard = RequestGuard::new(&req.model);

    // 4. Call backend (blueprint-specific)
    let result = backend.process.chat_completion(&req).await;

    match result {
        Ok(response) => {
            // 5. Calculate cost via shared CostModel
            let cost = backend.cost_model.calculate_cost(&CostParams {
                prompt_tokens: response.usage.prompt_tokens,
                completion_tokens: response.usage.completion_tokens,
                ..Default::default()
            });

            guard.record_tokens(
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
            );

            // 6. Settle via shared helper
            let _ = tangle_inference_core::settle_billing(
                &state.billing,
                &spend_auth,
                /*preauth=*/ spend_auth.amount,
                /*actual=*/ cost,
            ).await;

            Json(response).into_response()
        }
        Err(e) => error_response(e),
    }
}
```

### Step 8: Delete duplicated server boilerplate

The following code is now in core and can be deleted from your `server.rs`:

- `NonceStore` struct and impl → `tangle_inference_core::NonceStore`
- `SpendAuthPayload` struct → `tangle_inference_core::SpendAuthPayload`
- `AccountGuard` RAII → `tangle_inference_core::server::AccountGuard`
- `AppState` struct → `tangle_inference_core::AppState`
- `x402_payment_required()` helper → `tangle_inference_core::payment_required`
- `validate_spend_auth()` helper → `tangle_inference_core::validate_spend_auth`
- `X402_*` header constants → `tangle_inference_core::server::X402_*`
- EIP-712 signature recovery → handled internally by `validate_spend_auth`

### Step 9: Pick the right `CostModel` for your blueprint

| Blueprint | Cost Model | Why |
|---|---|---|
| LLM chat (llm-inference-blueprint, distributed-inference-blueprint) | `PerTokenCostModel` | Per-input-token + per-output-token pricing matches OpenAI-style billing |
| Text-to-speech (voice-inference-blueprint) | `PerCharCostModel` | TTS output is measured in characters, not tokens |
| Embeddings (embedding-inference-blueprint) | `PerTokenCostModel` | Per-1K-token pricing (use `price_per_input_token` for input tokens, 0 for output) |
| Image generation (image-gen-inference-blueprint) | `PerImageCostModel` | Flat per-image pricing |
| Video generation (video-gen-inference-blueprint) | `PerSecondCostModel` | Per-second-of-output-video pricing |
| Multi-task (modal-inference-blueprint) | `TaskTypeCostModel` | Composes multiple models by task type; Modal serves TTS+STT+image+video+fixed in one operator |

Example of `TaskTypeCostModel` composition for Modal:
```rust
use tangle_inference_core::{
    TaskTypeCostModel, PerTokenCostModel, PerCharCostModel,
    PerSecondCostModel, PerImageCostModel, FlatRequestCostModel,
    CostModel,
};
use std::collections::HashMap;

let mut per_task: HashMap<String, Box<dyn CostModel>> = HashMap::new();
per_task.insert("chat".into(), Box::new(PerTokenCostModel {
    price_per_input_token: 1,
    price_per_output_token: 3,
}));
per_task.insert("tts".into(), Box::new(PerCharCostModel {
    price_per_1k_chars: 15_000,
}));
per_task.insert("stt".into(), Box::new(PerSecondCostModel {
    price_per_second: 300,
}));
per_task.insert("image".into(), Box::new(PerImageCostModel {
    price_per_image: 50_000,
}));
per_task.insert("video".into(), Box::new(PerSecondCostModel {
    price_per_second: 1_000_000,
}));
per_task.insert("music".into(), Box::new(PerSecondCostModel {
    price_per_second: 500,
}));

let model = TaskTypeCostModel {
    default: Box::new(FlatRequestCostModel { price_per_request: 1000 }),
    per_task,
};

// In the handler:
let cost = model.calculate_cost(&CostParams {
    task_type: Some("tts".into()),
    extra: HashMap::from([("characters".into(), 500)]),
    ..Default::default()
});
```

### Step 10: Update docs

Update your `CLAUDE.md`, `PLAN.md`, and `README.md`:

```markdown
## Architecture

Depends on [`tangle-inference-core`](../tangle-inference-core/) for all shared
inference-operator infrastructure (billing, metrics, health, nonce store,
spend-auth validation, x402 payment headers, AppState builder). See
[`../tangle-inference-core/MIGRATION.md`](../tangle-inference-core/MIGRATION.md)
for the migration pattern.

The only truly <blueprint-name>-specific code is:
- `operator/src/<backend>.rs` — the backend subprocess/HTTP proxy
- `operator/src/server.rs` — the OpenAI-compatible HTTP handlers
- `operator/src/lib.rs` — the on-chain job handler (TangleArg/TangleResult)
- `contracts/` — the BSM contract and registration schema
```

### Step 11: Verify

```bash
cargo check -p <blueprint-name>    # must compile
cargo test -p <blueprint-name>     # must pass
cargo clippy -p <blueprint-name> -- -D warnings   # must be clean
```

Measure the LOC reduction:

```bash
wc -l operator/src/*.rs  # before vs after
```

## Common Pitfalls

### 1. `AppState` is no longer generic
`tangle-inference-core` had an earlier design where `AppState<B>` was generic over the backend type. **This was removed.** The current `AppState` is concrete, and the backend is attached via `Arc<dyn Any>`. Use `state.backend::<YourBackend>()` to retrieve it.

### 2. `NonceStore` is now async
Old `std::sync::RwLock` → new `tokio::sync::RwLock`. All `nonce_store.check_replay()` and `nonce_store.insert()` calls need `.await`.

### 3. `BillingClient::new` has two forms
- `new(&TangleConfig, &BillingConfig)` — convenience wrapper, use this if you already have the config structs
- `new_with_params(rpc_url, operator_key_hex, shielded_credits_address, service_id, max_gas_price_gwei)` — for blueprints that don't hold a unified config

### 4. Pricing moves from `billing.*` to `<backend>.*`
This is a deliberate wire-format break. The blueprint's unique value is its backend; pricing is part of the backend's contract. The `BillingConfig` in core only carries billing *infrastructure* params (contract address, gas cap, nonce store path), not per-token prices.

### 5. SpendAuth lives in x402 HTTP headers, not the request body
If your blueprint previously read `spend_auth` from the JSON body, update callers to use x402 headers:
```
X-Payment-Commitment: <hex>
X-Payment-Service-Id: <u64>
X-Payment-Amount: <u256>
X-Payment-Nonce: <u64>
X-Payment-Expiry: <u64>
X-Payment-Operator: <address>
X-Payment-Signature: <hex>
```
Use `extract_x402_spend_auth(&headers)` to parse.

### 6. Metric names are shared
Metrics like `tangle_operator_requests_total` are declared in core and shared across all blueprints. Prometheus dashboards scraping `tangle_operator_*` continue to work, but if your blueprint had custom metric names they need to be renamed.

## Migration Checklist

- [ ] Add `tangle-inference-core` to `operator/Cargo.toml`
- [ ] Delete `billing.rs` if present
- [ ] Delete `metrics.rs` if present
- [ ] Delete `health.rs` if present
- [ ] Rewrite `config.rs` to use core's config types
- [ ] Move pricing fields into your backend-specific config section
- [ ] Define a `<Backend>` struct holding runtime state + cost model
- [ ] Replace `AppState` usage with `AppStateBuilder` + `.backend(...)`
- [ ] Replace custom SpendAuth validation with `validate_spend_auth(&state, &auth).await`
- [ ] Replace custom x402 helpers with `extract_x402_spend_auth`, `payment_required`, `error_response`
- [ ] Replace custom `NonceStore` with `tangle_inference_core::NonceStore`
- [ ] Replace custom metrics with `RequestGuard`, `gather`, `on_chain_metrics`
- [ ] Update all `.await` points on `NonceStore` methods (now async)
- [ ] Update `CLAUDE.md`, `PLAN.md`, `README.md`
- [ ] Update `deploy/config.example.json` if pricing moved
- [ ] `cargo check` clean
- [ ] `cargo test` passing
- [ ] `cargo clippy -- -D warnings` clean
- [ ] Measure LOC reduction (`wc -l operator/src/*.rs`)

## Expected LOC Reduction

Based on the `llm-inference-blueprint` reference migration:

| Blueprint | Before | After (estimated) | Reduction |
|---|---|---|---|
| llm-inference-blueprint (done) | 3,640 | 1,819 | −1,821 (50%) |
| voice-inference-blueprint | ~2,553 | ~1,300 | −1,253 (49%) |
| embedding-inference-blueprint | ~2,169 | ~1,100 | −1,069 (49%) |
| modal-inference-blueprint | ~3,011 | ~1,700 | −1,311 (44%) |
| image-gen-inference-blueprint | ~1,999 | ~1,100 | −899 (45%) |
| video-gen-inference-blueprint | ~2,606 | ~1,400 | −1,206 (46%) |
| distributed-inference-blueprint | ~2,166 | ~1,200 | −966 (45%) |
| **Total** | **~18,144** | **~9,619** | **−8,525 (47%)** |
