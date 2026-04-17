//! Minimal inference operator wired with `tangle-inference-core`.
//!
//! This example shows the smallest possible blueprint operator: it accepts
//! `POST /v1/chat/completions`, validates an x402/SpendAuth payment, computes
//! a per-token cost, and returns a stub response. A real blueprint would
//! replace `EchoBackend` with its inference client (vLLM, Modal, llama.cpp,
//! Ollama, ...) and call into it instead of echoing.
//!
//! Run with:
//! ```bash
//! cargo run --example minimal_operator
//! ```
//!
//! It will fail at startup because no Tangle RPC is available — that's
//! expected. The point of the example is to show the wiring.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use tangle_inference_core::{
    AppState, AppStateBuilder, BillingClient, BillingConfig, CostModel, CostParams, NonceStore,
    PerTokenCostModel, ServerConfig, SpendAuthPayload, TangleConfig,
};

// === Blueprint-specific backend ====================================
//
// Replace this with your real inference client. Anything `Send + Sync +
// 'static` works — a vLLM child-process handle, a Modal HTTP client, an
// Ollama wrapper, etc. The backend is attached to `AppState` via
// `AppStateBuilder::backend(...)` and retrieved in handlers via
// `state.backend::<EchoBackend>().unwrap()`.

#[derive(Clone)]
struct EchoBackend {
    model_name: String,
}

impl EchoBackend {
    fn run_chat(&self, prompt: &str) -> (String, u32, u32) {
        let reply = format!("[{}] echo: {prompt}", self.model_name);
        let prompt_tokens = prompt.split_whitespace().count() as u32;
        let completion_tokens = reply.split_whitespace().count() as u32;
        (reply, prompt_tokens, completion_tokens)
    }
}

// === Wire format ====================================================

#[derive(Deserialize)]
struct ChatRequest {
    prompt: String,
    spend_auth: SpendAuthPayload,
}

#[derive(Serialize)]
struct ChatResponse {
    text: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost: u64,
}

// === Handler ========================================================

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, String)> {
    // Backend-specific work — pull our concrete backend out of AppState.
    let backend = state
        .backend::<EchoBackend>()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "missing backend".into()))?;

    // (In a real blueprint you would call `validate_spend_auth(&state, &req.spend_auth).await`
    // here and return its error response on failure. Skipped in this example
    // because it requires a live Tangle RPC.)
    let _ = &req.spend_auth;

    let (text, prompt_tokens, completion_tokens) = backend.run_chat(&req.prompt);

    // Per-token pricing — the most common LLM cost model.
    let cost = PerTokenCostModel {
        price_per_input_token: 2,
        price_per_output_token: 5,
    }
    .calculate_cost(&CostParams {
        prompt_tokens,
        completion_tokens,
        ..Default::default()
    });

    Ok(Json(ChatResponse {
        text,
        prompt_tokens,
        completion_tokens,
        cost,
    }))
}

async fn health() -> &'static str {
    "ok"
}

// === Bootstrap ======================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();

    // Config (would normally come from a TOML file or env vars).
    let tangle = TangleConfig {
        rpc_url: "http://localhost:8545".into(),
        chain_id: 31337,
        operator_key: "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".into(),
        shielded_credits: "0x5FbDB2315678afecb367f032d93F642f64180aa3".into(),
        blueprint_id: 1,
        service_id: Some(42),
    };
    let billing_cfg = BillingConfig {
        payment_mode: tangle_inference_core::PaymentMode::Shielded,
        billing_required: true,
        max_spend_per_request: 1_000_000,
        min_credit_balance: 1_000,
        min_charge_amount: 100,
        claim_max_retries: 3,
        clock_skew_tolerance_secs: 30,
        max_gas_price_gwei: 0,
        nonce_store_path: None,
        payment_token_address: None,
    };
    let server_cfg = ServerConfig {
        host: "0.0.0.0".into(),
        port: 8080,
        max_concurrent_requests: 32,
        max_request_body_bytes: 16 * 1024 * 1024,
        stream_timeout_secs: 300,
        idle_chunk_timeout_secs: 30,
        max_line_buf_bytes: 1024 * 1024,
        max_per_account_requests: 0,
    };

    // Construct billing client. The convenience constructor below uses the
    // full config structs; a blueprint without those structs can call
    // `BillingClient::new_with_params(...)` instead.
    let billing = BillingClient::new(&tangle, &billing_cfg)?;
    let operator_address = billing.operator_address();

    let nonce_store = Arc::new(NonceStore::load(billing_cfg.nonce_store_path.clone()));

    // Build AppState. The backend is opaque to the crate — plug in whatever
    // type your blueprint needs.
    let state = AppStateBuilder::new()
        .billing(Arc::new(billing))
        .nonce_store(nonce_store)
        .server_config(Arc::new(server_cfg.clone()))
        .billing_config(Arc::new(billing_cfg))
        .tangle_config(Arc::new(tangle))
        .operator_address(operator_address)
        .backend(EchoBackend {
            model_name: "echo-1".into(),
        })
        .build()?;

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/health", get(health))
        .with_state(state);

    let bind = format!("{}:{}", server_cfg.host, server_cfg.port);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "minimal_operator listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn tracing_subscriber_init() {
    // Examples shouldn't pull a heavy subscriber dep; this is enough.
    let _ = std::panic::catch_unwind(|| {
        tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default())
    });
}
