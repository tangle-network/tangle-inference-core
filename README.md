# tangle-inference-core

Shared infrastructure for Tangle inference blueprints. This crate exists so
that every inference blueprint (vLLM, llama.cpp, Modal, Ollama, Replicate,
TGI, and friends) can share the parts they all need — billing, x402/SpendAuth
validation, replay-protected nonces, Prometheus metrics, GPU detection, and
HTTP plumbing — without copy-pasting the same 2k lines into every operator.

The crate is **backend-agnostic**: it knows nothing about your inference
runtime. You plug your runtime in via a builder and it appears in your
handlers as an opaque extension.

---

## What's in the box

| Module | Purpose |
| --- | --- |
| `billing` | `BillingClient` (ShieldedCredits on-chain), `CostModel` trait + 6 concrete implementations, EIP-712 SpendAuth signature recovery |
| `server` | `AppState` + `AppStateBuilder`, `NonceStore` (replay protection with file persistence), x402 helpers, `validate_spend_auth`, `settle_billing` |
| `metrics` | Prometheus registry, `RequestGuard` RAII tracker, on-chain QoS snapshots |
| `health` | `nvidia-smi` parsing and GPU detection |
| `config` | `TangleConfig`, `BillingConfig`, `ServerConfig`, `GpuConfig` |

---

## Adding the dependency

Until this is published to crates.io, depend on the git repo or a local path:

```toml
[dependencies]
tangle-inference-core = { git = "https://github.com/tangle-network/tangle-inference-core", branch = "main" }

# Or, while iterating in a workspace:
tangle-inference-core = { path = "../tangle-inference-core" }
```

The crate re-exports the most common items at the top level:

```rust
use tangle_inference_core::{
    AppState, AppStateBuilder, BillingClient, BillingConfig, CostModel, CostParams,
    NonceStore, PerTokenCostModel, ServerConfig, SpendAuthPayload, TangleConfig,
};
```

---

## Step-by-step adoption (replacing your operator's billing/server/metrics)

If you have an existing inference blueprint operator, the migration is
mechanical. Here's the recipe:

### 1. Delete `operator/src/billing.rs`

Replace `use crate::billing::BillingClient` with
`use tangle_inference_core::BillingClient`. Construct it via either:

```rust
// Convenience wrapper that takes the full config structs.
let billing = BillingClient::new(&tangle_config, &billing_config)?;

// Or, if your blueprint doesn't use `TangleConfig` / `BillingConfig`:
let billing = BillingClient::new_with_params(
    rpc_url.into(),
    operator_key_hex.into(),
    shielded_credits_address,
    service_id,
    max_gas_price_gwei,
)?;
```

The decoupled constructor is the right choice for blueprints that have their
own config layout (Modal, Replicate) — there's no need to invent fake
`TangleConfig` fields just to call into the crate.

### 2. Replace `AppState<MyBackend>` boilerplate

Before:

```rust
#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<VllmProcess>,
    pub billing: Arc<BillingClient>,
    pub server_config: ServerConfig,
    pub billing_config: BillingConfig,
    pub tangle_config: TangleConfig,
    pub semaphore: Arc<tokio::sync::Semaphore>,
    pub nonce_store: Arc<NonceStore>,
    pub active_per_account: Arc<RwLock<HashMap<String, usize>>>,
    pub operator_address: Address,
}
```

After:

```rust
use tangle_inference_core::{AppState, AppStateBuilder};

let state: AppState = AppStateBuilder::new()
    .billing(Arc::new(billing))
    .nonce_store(Arc::new(NonceStore::load(billing_cfg.nonce_store_path.clone())))
    .server_config(Arc::new(server_cfg))
    .billing_config(Arc::new(billing_cfg))
    .tangle_config(Arc::new(tangle_cfg))
    .operator_address(operator_address)
    .max_concurrent(64)
    .backend(VllmProcess::spawn(...).await?)
    .build()?;
```

In your handlers, retrieve the backend with `state.backend::<VllmProcess>()`:

```rust
async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    let vllm = state.backend::<VllmProcess>().expect("backend type mismatch");
    vllm.generate(...).await
}
```

The downcast is `O(1)` and infallible if you constructed the state with the
matching type — `expect` is fine because the mismatch is a programmer error,
not user input.

### 3. Replace `operator/src/server.rs` glue

`tangle-inference-core::server` already provides:

- `validate_spend_auth(&state, &spend_auth)` — full pre-flight validation
  (amount, min charge, max spend, operator match, service id, nonce replay,
  EIP-712 signer recovery, on-chain spending key + balance check). Returns
  `Ok(amount)` or a ready-to-return `axum::response::Response` with the
  correct 4xx status.
- `settle_billing(&billing, &spend_auth, preauth_amount, actual_cost)` — caps
  actual cost at the pre-auth amount and calls `claimPayment` on-chain with
  retries.
- `extract_x402_spend_auth(&headers)` — parse `X-Payment-Signature` header.
- `payment_required(...)` — build a 402 response with all the x402 headers.
- `error_response(status, msg, type, code)` — OpenAI-compatible error JSON.
- `NonceStore` — persistent nonce ledger; use it as
  `Arc<NonceStore>` in your `AppState`.

A typical request flow becomes:

```rust
async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ChatRequest>,
) -> Response {
    let spend_auth = req.spend_auth.take()
        .or_else(|| extract_x402_spend_auth(&headers));
    let Some(spend_auth) = spend_auth else {
        return payment_required(&state.billing_config, &state.tangle_config,
            state.operator_address, state.billing_config.min_charge_amount);
    };

    let preauth = match validate_spend_auth(&state, &spend_auth).await {
        Ok(amount) => amount,
        Err(resp) => return resp,
    };

    let backend = state.backend::<VllmProcess>().unwrap();
    let result = backend.generate(&req).await;

    let cost = PerTokenCostModel { price_per_input_token: 2, price_per_output_token: 5 }
        .calculate_cost(&CostParams {
            prompt_tokens: result.prompt_tokens,
            completion_tokens: result.completion_tokens,
            ..Default::default()
        });
    settle_billing(&state.billing, &spend_auth, preauth, cost).await;

    Json(result).into_response()
}
```

### 4. Replace `operator/src/metrics.rs`

Drop your local Prometheus registry and use the crate's. Wrap each request in
a `RequestGuard`:

```rust
use tangle_inference_core::RequestGuard;
use tangle_inference_core::metrics::{gather, on_chain_metrics, health_summary};

let mut guard = RequestGuard::new("Llama-3.1-8B-Instruct");
// ... do work ...
guard.set_tokens(prompt_tokens, completion_tokens);
guard.set_ttft(120);
guard.set_success();
// drop(guard) records everything
```

Then expose `/metrics` with `gather()` and `/health` with `health_summary()`.
For on-chain QoS submission, call `on_chain_metrics()`.

### 5. Replace `operator/src/health.rs` GPU code

```rust
use tangle_inference_core::{detect_gpus, GpuInfo};

let gpus: Vec<GpuInfo> = detect_gpus().await?;
```

---

## Migration impact (vLLM blueprint estimate)

Counting LOC removed from `vllm-inference-blueprint/operator/src/` when each
module is replaced by the crate equivalent:

| File | Before | After (delta to imports) | Removed |
| --- | --- | --- | --- |
| `billing.rs` | ~430 | ~10 | **-420** |
| `server.rs` (boilerplate only — handlers stay) | ~460 | ~70 | **-390** |
| `metrics.rs` | ~470 | ~5 | **-465** |
| `health.rs` (GPU detection) | ~130 | ~5 | **-125** |
| `config.rs` (Tangle/Billing/Server structs) | ~200 | ~20 (blueprint-specific extensions) | **-180** |
| **Total** | **~1690** | **~110** | **~-1580** |

Across the seven inference blueprints that's roughly **11k lines** of
duplicate operator code that disappears, while every fix to billing or
metrics now ships to all of them at once.

---

## CostModel — pricing your inference

The `CostModel` trait is dead simple:

```rust
pub trait CostModel: Send + Sync + 'static {
    fn calculate_cost(&self, params: &CostParams) -> u64;
}

pub struct CostParams {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub task_type: Option<String>,                    // for task-aware pricing
    pub extra: HashMap<String, u64>,                  // backend-specific inputs
}
```

The crate ships six concrete implementations covering every inference task
type the blueprints currently serve:

| Implementation | Use case | Reads from |
| --- | --- | --- |
| `PerTokenCostModel` | LLM chat / completion | `prompt_tokens`, `completion_tokens` |
| `PerCharCostModel` | TTS (text → audio) | `extra["characters"]` |
| `PerSecondCostModel` | STT, video generation | `extra["centiseconds"]` (= 1/100s) |
| `PerImageCostModel` | Image generation | `extra["images"]` (default 1) |
| `FlatRequestCostModel` | Embeddings, classification | nothing |
| `TaskTypeCostModel` | Mixed-task operators (Modal) | `task_type`, dispatches to a sub-model |

`TaskTypeCostModel` is the pattern Modal uses: one operator, many task types
under one billing root. You compose it like this:

```rust
use std::collections::HashMap;
use tangle_inference_core::{
    CostModel, PerCharCostModel, PerImageCostModel, PerTokenCostModel, TaskTypeCostModel,
};

let mut per_task: HashMap<String, Box<dyn CostModel>> = HashMap::new();
per_task.insert("tts".into(), Box::new(PerCharCostModel { price_per_1k_chars: 4_000 }));
per_task.insert("image".into(), Box::new(PerImageCostModel { price_per_image: 50_000 }));

let model = TaskTypeCostModel {
    default: Box::new(PerTokenCostModel { price_per_input_token: 2, price_per_output_token: 5 }),
    per_task,
};
```

If you need a custom model (e.g. step-aware pricing for diffusion), just
implement `CostModel` yourself — the trait is intentionally trivial.

---

## AppStateBuilder reference

Required:

- `billing(Arc<BillingClient>)`
- `nonce_store(Arc<NonceStore>)`
- `server_config(Arc<ServerConfig>)`
- `billing_config(Arc<BillingConfig>)`
- `tangle_config(Arc<TangleConfig>)`
- `operator_address(Address)`
- `backend<B: Send + Sync + 'static>(B)`

Optional:

- `max_concurrent(usize)` — defaults to `server_config.max_concurrent_requests`.

`build()` returns `anyhow::Result<AppState>`. Missing required fields produce
a clear error message naming the missing field — no panics.

`AppState` is `Clone` (everything is `Arc`-wrapped), so you can pass it to
`Router::with_state(state)` and clone it freely into spawned tasks.

---

## Plugging in a backend

The backend is `Arc<dyn Any + Send + Sync>` under the hood, so any type works:

```rust
struct VllmProcess { /* child handle, http client, ... */ }

let state = AppStateBuilder::new()
    /* ... */
    .backend(VllmProcess::spawn().await?)
    .build()?;
```

In handlers:

```rust
async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    let vllm: &VllmProcess = state.backend::<VllmProcess>().unwrap();
    vllm.generate(...).await
}
```

If you need multiple backends (rare), wrap them in your own struct and store
that struct as the backend.

---

## Running the example

```bash
cargo run --example minimal_operator
```

This is an end-to-end EchoBackend operator that wires every part of the
crate. It will fail at startup because there's no Tangle RPC at
`localhost:8545` — that's expected. The point is the wiring; copy the
`build()` call into your own operator's `main`.

---

## Testing

```bash
cargo test
```

The integration test (`tests/integration_test.rs`) constructs a real
`AppState` via the builder, exercises every `CostModel`, signs a
`SpendAuthPayload` with a known Anvil keypair, and round-trips it through
`recover_spend_auth_signer` / `verify_spend_auth_signature`. It also tests
that the persistent `NonceStore` survives reload.
