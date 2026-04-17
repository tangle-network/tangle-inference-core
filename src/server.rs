use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alloy::primitives::Address;
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::billing::BillingClient;
use crate::config::{BillingConfig, ServerConfig, TangleConfig};
use crate::payment::{PaymentProvider, PaymentProof, create_provider};
use crate::settlement_queue::{FailedSettlement, SettlementRecoveryQueue};

// x402 header constants
pub const X402_PAYMENT_REQUIRED: &str = "X-Payment-Required";
pub const X402_PAYMENT_TOKEN: &str = "X-Payment-Token";
pub const X402_PAYMENT_RECIPIENT: &str = "X-Payment-Recipient";
pub const X402_PAYMENT_NETWORK: &str = "X-Payment-Network";
pub const X402_PAYMENT_SIGNATURE: &str = "X-Payment-Signature";

/// Nonce key: (commitment, nonce) pair for replay protection.
pub type NonceKey = (String, u64);

#[derive(Serialize, Deserialize)]
struct NonceRecord {
    commitment: String,
    nonce: u64,
    expiry: u64,
}

/// Replay-protection nonce store with optional file persistence.
///
/// Uses `tokio::sync::RwLock` for async-first access patterns.
/// Without persistence (`nonce_store_path` unset), operator restarts clear all
/// nonces, allowing replay of any unexpired SpendAuth signatures.
pub struct NonceStore {
    nonces: RwLock<HashMap<NonceKey, u64>>,
    path: Option<PathBuf>,
}

impl NonceStore {
    /// Create a new nonce store, loading persisted nonces from disk if path is set.
    pub fn load(path: Option<PathBuf>) -> Self {
        let nonces: HashMap<NonceKey, u64> = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|data| serde_json::from_str::<Vec<NonceRecord>>(&data).ok())
            .map(|records| {
                records
                    .into_iter()
                    .map(|r| ((r.commitment, r.nonce), r.expiry))
                    .collect()
            })
            .unwrap_or_default();

        if path.is_some() {
            tracing::info!(count = nonces.len(), "loaded persisted nonces");
        } else {
            tracing::warn!(
                "nonce_store_path not configured — nonces are in-memory only. \
                 Operator restart will allow replay of unexpired SpendAuth signatures."
            );
        }

        Self {
            nonces: RwLock::new(nonces),
            path,
        }
    }

    /// Check if the nonce is already used. Does NOT insert — use
    /// `check_and_insert` for the atomic check+insert pattern.
    pub async fn check_replay(&self, key: &NonceKey, tolerance: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut nonces = self.nonces.write().await;
        nonces.retain(|_, expiry| now <= expiry.saturating_add(tolerance));
        nonces.contains_key(key)
    }

    /// Atomically check if a nonce is already used AND insert it if not.
    /// Returns `true` if the nonce was already present (replay detected).
    /// Returns `false` if the nonce was fresh and has been recorded.
    ///
    /// This is the primary API for replay protection — it holds the write
    /// lock across both the check and the insert, preventing TOCTOU races
    /// where two concurrent requests with the same nonce could both pass
    /// a separate check_replay before either inserts.
    pub async fn check_and_insert(&self, key: NonceKey, expiry: u64, tolerance: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut nonces = self.nonces.write().await;
        nonces.retain(|_, exp| now <= exp.saturating_add(tolerance));
        if nonces.contains_key(&key) {
            return true; // replay
        }
        nonces.insert(key, expiry);
        self.persist(&nonces);
        false // fresh
    }

    /// Record a nonce as used, evict expired entries, and persist to disk.
    /// Prefer `check_and_insert` for the atomic check+insert pattern.
    pub async fn insert(&self, key: NonceKey, expiry: u64, tolerance: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut nonces = self.nonces.write().await;
        nonces.retain(|_, exp| now <= exp.saturating_add(tolerance));
        nonces.insert(key, expiry);
        self.persist(&nonces);
    }

    fn persist(&self, nonces: &HashMap<NonceKey, u64>) {
        let Some(ref path) = self.path else { return };
        let records: Vec<NonceRecord> = nonces
            .iter()
            .map(|((commitment, nonce), expiry)| NonceRecord {
                commitment: commitment.clone(),
                nonce: *nonce,
                expiry: *expiry,
            })
            .collect();
        let Ok(data) = serde_json::to_string(&records) else {
            tracing::error!("failed to serialize nonce store");
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &data).is_ok() {
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!(error = %e, "failed to persist nonce store");
            }
        }
    }
}

/// RAII guard that decrements the per-account active request count on drop.
pub struct AccountGuard {
    commitment: String,
    active: Arc<RwLock<HashMap<String, usize>>>,
}

impl AccountGuard {
    /// Create a guard that increments the per-account counter NOW and
    /// decrements it on drop. This ensures the counter is always balanced
    /// regardless of early returns or panics in the caller.
    pub async fn acquire(
        commitment: String,
        active: Arc<RwLock<HashMap<String, usize>>>,
        max_per_account: usize,
    ) -> Option<Self> {
        let mut map = active.write().await;
        let count = map.entry(commitment.clone()).or_insert(0);
        if max_per_account > 0 && *count >= max_per_account {
            return None; // at capacity
        }
        *count += 1;
        drop(map);
        Some(Self { commitment, active })
    }

    /// Create a guard without checking capacity (for backwards compat).
    pub fn new(commitment: String, active: Arc<RwLock<HashMap<String, usize>>>) -> Self {
        Self { commitment, active }
    }
}

impl Drop for AccountGuard {
    fn drop(&mut self) {
        // Use try_write to avoid blocking in drop; if contended, spawn a task.
        match self.active.try_write() {
            Ok(mut map) => {
                if let Some(count) = map.get_mut(&self.commitment) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&self.commitment);
                    }
                }
            }
            Err(_) => {
                let commitment = self.commitment.clone();
                let active = Arc::clone(&self.active);
                tokio::spawn(async move {
                    let mut map = active.write().await;
                    if let Some(count) = map.get_mut(&commitment) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            map.remove(&commitment);
                        }
                    }
                });
            }
        }
    }
}

/// Shared application state.
///
/// Backend-specific state (vLLM process, Modal client, etc.) is attached as
/// an opaque `Arc<dyn Any + Send + Sync>` and retrieved via [`AppState::backend`].
/// Construct via [`AppStateBuilder`].
#[derive(Clone)]
pub struct AppState {
    pub billing: Arc<BillingClient>,
    pub nonce_store: Arc<NonceStore>,
    pub server_config: Arc<ServerConfig>,
    pub billing_config: Arc<BillingConfig>,
    pub tangle_config: Arc<TangleConfig>,
    pub operator_address: Address,
    pub semaphore: Arc<tokio::sync::Semaphore>,
    pub active_per_account: Arc<RwLock<HashMap<String, usize>>>,
    /// Persistent dead-letter queue for failed on-chain settlements.
    pub settlement_recovery_queue: Option<Arc<SettlementRecoveryQueue>>,
    /// Backend-specific state. Downcast via [`AppState::backend`].
    pub backend: Arc<dyn std::any::Any + Send + Sync>,
    /// Generic payment provider (replaces direct BillingClient usage).
    /// Supports ShieldedCredits, DirectTransfer, or NoOp depending on config.
    /// New blueprints should use this instead of `billing` directly.
    pub payment_provider: Arc<dyn PaymentProvider>,
}

impl AppState {
    /// Downcast the backend extension to a concrete type.
    ///
    /// Returns `None` if the stored backend is not of type `B`.
    pub fn backend<B: 'static>(&self) -> Option<&B> {
        self.backend.downcast_ref::<B>()
    }

    /// Convenience constructor that wires up BillingClient, NonceStore, and
    /// operator address from config structs. Blueprints only need to provide
    /// their backend and max_concurrent setting.
    pub fn from_config<B: Send + Sync + 'static>(
        tangle: &TangleConfig,
        server: &ServerConfig,
        billing: &BillingConfig,
        max_concurrent: usize,
        backend: B,
    ) -> anyhow::Result<Self> {
        let billing_client = BillingClient::new(tangle, billing)?;
        let operator_address = billing_client.operator_address();
        let nonce_store = Arc::new(NonceStore::load(billing.nonce_store_path.clone()));

        // Create payment provider from config's payment_mode
        let provider = create_provider(billing.payment_mode, tangle, billing)?;

        AppStateBuilder::new()
            .billing(Arc::new(billing_client))
            .nonce_store(nonce_store)
            .server_config(Arc::new(server.clone()))
            .billing_config(Arc::new(billing.clone()))
            .tangle_config(Arc::new(tangle.clone()))
            .operator_address(operator_address)
            .max_concurrent(max_concurrent)
            .payment_provider(Arc::from(provider) as Arc<dyn PaymentProvider>)
            .backend(backend)
            .build()
    }
}

/// Builder for [`AppState`]. Required fields: `billing`, `nonce_store`,
/// `server_config`, `billing_config`, `tangle_config`, `operator_address`,
/// `backend`. `max_concurrent` defaults to `server_config.max_concurrent_requests`.
#[derive(Default)]
pub struct AppStateBuilder {
    billing: Option<Arc<BillingClient>>,
    nonce_store: Option<Arc<NonceStore>>,
    server_config: Option<Arc<ServerConfig>>,
    billing_config: Option<Arc<BillingConfig>>,
    tangle_config: Option<Arc<TangleConfig>>,
    operator_address: Option<Address>,
    max_concurrent: Option<usize>,
    settlement_recovery_queue: Option<Arc<SettlementRecoveryQueue>>,
    payment_provider: Option<Arc<dyn PaymentProvider>>,
    backend: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl AppStateBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn billing(mut self, b: Arc<BillingClient>) -> Self {
        self.billing = Some(b);
        self
    }

    pub fn nonce_store(mut self, n: Arc<NonceStore>) -> Self {
        self.nonce_store = Some(n);
        self
    }

    pub fn server_config(mut self, c: Arc<ServerConfig>) -> Self {
        self.server_config = Some(c);
        self
    }

    pub fn billing_config(mut self, c: Arc<BillingConfig>) -> Self {
        self.billing_config = Some(c);
        self
    }

    pub fn tangle_config(mut self, c: Arc<TangleConfig>) -> Self {
        self.tangle_config = Some(c);
        self
    }

    pub fn operator_address(mut self, a: Address) -> Self {
        self.operator_address = Some(a);
        self
    }

    pub fn max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = Some(n);
        self
    }

    pub fn settlement_recovery_queue(mut self, q: Arc<SettlementRecoveryQueue>) -> Self {
        self.settlement_recovery_queue = Some(q);
        self
    }

    pub fn payment_provider(mut self, p: Arc<dyn PaymentProvider>) -> Self {
        self.payment_provider = Some(p);
        self
    }

    /// Attach a backend value of any type. Retrieve in handlers via
    /// `state.backend::<MyBackend>().unwrap()`.
    pub fn backend<B: Send + Sync + 'static>(mut self, b: B) -> Self {
        self.backend = Some(Arc::new(b));
        self
    }

    pub fn build(self) -> anyhow::Result<AppState> {
        let billing = self
            .billing
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: billing is required"))?;
        let nonce_store = self
            .nonce_store
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: nonce_store is required"))?;
        let server_config = self
            .server_config
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: server_config is required"))?;
        let billing_config = self
            .billing_config
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: billing_config is required"))?;
        let tangle_config = self
            .tangle_config
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: tangle_config is required"))?;
        let operator_address = self
            .operator_address
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: operator_address is required"))?;
        let backend = self
            .backend
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: backend is required"))?;

        let max_concurrent = self
            .max_concurrent
            .unwrap_or(server_config.max_concurrent_requests);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

        // Payment provider: use explicit if provided, otherwise create NoopProvider
        // as fallback (blueprints using from_config always get the right one).
        let payment_provider = match self.payment_provider {
            Some(p) => p,
            None => {
                // Fallback: create from config if available, or noop
                let mode = billing_config.payment_mode;
                match create_provider(mode, &tangle_config, &billing_config) {
                    Ok(p) => {
                        let p: Arc<dyn PaymentProvider> = Arc::from(p);
                        p
                    }
                    Err(_) => {
                        // If provider creation fails (e.g. Direct without token),
                        // fall back to a ShieldedProvider wrapping the billing client.
                        Arc::new(crate::payment::ShieldedProvider::new(&tangle_config, &billing_config)?)
                            as Arc<dyn PaymentProvider>
                    }
                }
            }
        };

        Ok(AppState {
            billing,
            nonce_store,
            server_config,
            billing_config,
            tangle_config,
            operator_address,
            semaphore,
            active_per_account: Arc::new(RwLock::new(HashMap::new())),
            settlement_recovery_queue: self.settlement_recovery_queue,
            payment_provider,
            backend,
        })
    }
}

/// Deserialize a u64 that may be JSON number or string.
/// JS bigints serialize as JSON strings; this lets the Rust operator accept
/// both the number form (from Rust/Go clients) and the string form (from the
/// @tangle-network/tcloud SDK which emits `nonce: "0"`, `serviceId: "1"`, etc).
fn de_u64_flex<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    use serde::Deserialize as _;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum N {
        Num(u64),
        Str(String),
    }
    match N::deserialize(deserializer)? {
        N::Num(n) => Ok(n),
        N::Str(s) => s.parse::<u64>().map_err(serde::de::Error::custom),
    }
}

fn de_u8_flex<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    use serde::Deserialize as _;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum N {
        Num(u8),
        Str(String),
    }
    match N::deserialize(deserializer)? {
        N::Num(n) => Ok(n),
        N::Str(s) => s.parse::<u8>().map_err(serde::de::Error::custom),
    }
}

/// ShieldedCredits spend authorization payload.
///
/// Accepts both camelCase (from JS SDK) and snake_case field names via
/// `rename_all`. Numeric fields accept JSON number OR string form — the JS
/// SDK serializes bigints as strings for JSON safety.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpendAuthPayload {
    pub commitment: String,
    #[serde(deserialize_with = "de_u64_flex")]
    pub service_id: u64,
    #[serde(deserialize_with = "de_u8_flex")]
    pub job_index: u8,
    pub amount: String,
    pub operator: String,
    #[serde(deserialize_with = "de_u64_flex")]
    pub nonce: u64,
    #[serde(deserialize_with = "de_u64_flex")]
    pub expiry: u64,
    pub signature: String,
}

/// Standard error response body (OpenAI-compatible).
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

/// Error detail within an ErrorResponse.
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    pub r#type: String,
    pub code: String,
}

/// Build a standard JSON error response.
pub fn error_response(
    status: StatusCode,
    message: String,
    error_type: &str,
    code: &str,
) -> Response {
    let body = ErrorResponse {
        error: ErrorDetail {
            message,
            r#type: error_type.to_string(),
            code: code.to_string(),
        },
    };
    (status, Json(body)).into_response()
}

/// Build a 402 Payment Required response with x402 headers.
pub fn payment_required(
    billing_config: &BillingConfig,
    tangle_config: &TangleConfig,
    operator_address: Address,
    estimated_amount: u64,
) -> Response {
    let amount = estimated_amount.max(billing_config.min_charge_amount);

    let body = serde_json::json!({
        "error": "payment_required",
        "amount": amount.to_string(),
        "token": billing_config.payment_token_address.as_deref()
            .unwrap_or("0x0000000000000000000000000000000000000000"),
        "recipient": format!("{}", operator_address),
        "network": tangle_config.chain_id.to_string(),
        "accepts": ["spend_auth"],
        "description": "ShieldedCredits SpendAuth required. Include spend_auth in request body or X-Payment-Signature header."
    });

    Response::builder()
        .status(StatusCode::PAYMENT_REQUIRED)
        .header(header::CONTENT_TYPE, "application/json")
        .header(X402_PAYMENT_REQUIRED, amount.to_string())
        .header(
            X402_PAYMENT_TOKEN,
            billing_config
                .payment_token_address
                .as_deref()
                .unwrap_or("0x0000000000000000000000000000000000000000"),
        )
        .header(X402_PAYMENT_RECIPIENT, format!("{}", operator_address))
        .header(X402_PAYMENT_NETWORK, tangle_config.chain_id.to_string())
        .body(Body::from(serde_json::to_string(&body).unwrap_or_default()))
        .unwrap_or_else(|e| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build 402 response: {e}"),
                "internal_error",
                "response_build_failed",
            )
        })
}

/// Try to extract a SpendAuth from x402 headers (X-Payment-Signature).
/// The header value is JSON or hex-encoded JSON.
pub fn extract_x402_spend_auth(headers: &HeaderMap) -> Option<SpendAuthPayload> {
    let header_val = headers.get(X402_PAYMENT_SIGNATURE)?.to_str().ok()?;

    // Try direct JSON first
    if let Ok(payload) = serde_json::from_str::<SpendAuthPayload>(header_val) {
        return Some(payload);
    }

    // Try hex-encoded JSON
    let hex_str = header_val.strip_prefix("0x").unwrap_or(header_val);
    if let Ok(decoded) = hex::decode(hex_str) {
        if let Ok(payload) = serde_json::from_slice::<SpendAuthPayload>(&decoded) {
            return Some(payload);
        }
    }

    None
}

/// Validate a SpendAuth payload against operator configuration.
///
/// Checks: amount parsing, min charge, max spend, operator address match,
/// service ID match, nonce replay. Returns an error response on any failure,
/// or Ok(parsed_amount) on success.
pub async fn validate_spend_auth(
    state: &AppState,
    spend_auth: &SpendAuthPayload,
) -> Result<u64, Response> {
    // Parse amount
    let requested_amount: u64 = spend_auth.amount.parse().map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid spend_auth amount: must be a valid u64 integer".to_string(),
            "billing_error",
            "invalid_amount",
        )
    })?;

    // Min charge
    let min_charge = state.billing_config.min_charge_amount;
    if min_charge > 0 && requested_amount < min_charge {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("spend authorization amount ({requested_amount}) is below minimum charge ({min_charge})"),
            "billing_error",
            "below_min_charge",
        ));
    }

    // Max spend
    let max_spend = state.billing_config.max_spend_per_request;
    if max_spend > 0 && requested_amount > max_spend {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("spend authorization amount ({requested_amount}) exceeds max_spend_per_request ({max_spend})"),
            "billing_error",
            "exceeds_max_spend",
        ));
    }

    // Operator address match
    let spend_operator: Address = spend_auth.operator.parse().map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid operator address in spend_auth".to_string(),
            "billing_error",
            "invalid_operator",
        )
    })?;
    if spend_operator != state.operator_address {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "spend_auth operator ({spend_operator}) does not match this operator ({})",
                state.operator_address
            ),
            "billing_error",
            "operator_mismatch",
        ));
    }

    // Service ID match
    if let Some(expected_service_id) = state.tangle_config.service_id {
        if spend_auth.service_id != expected_service_id {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "spend_auth service_id ({}) does not match operator service ({expected_service_id})",
                    spend_auth.service_id
                ),
                "billing_error",
                "service_id_mismatch",
            ));
        }
    }

    // Atomic nonce check + insert — holds the write lock across both
    // operations to prevent TOCTOU races where two concurrent requests
    // with the same nonce could both pass a separate check before either
    // inserts. If downstream validation fails, we've "wasted" a nonce,
    // which is safe (the client can always generate a new one).
    let nonce_key = (spend_auth.commitment.clone(), spend_auth.nonce);
    if state
        .nonce_store
        .check_and_insert(
            nonce_key,
            spend_auth.expiry,
            state.billing_config.clock_skew_tolerance_secs,
        )
        .await
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "spend_auth nonce already used (replay detected)".to_string(),
            "billing_error",
            "nonce_replay",
        ));
    }

    // Recover signer
    let recovered_address = crate::billing::recover_spend_auth_signer(
        spend_auth,
        &state.tangle_config.shielded_credits,
        state.tangle_config.chain_id,
        state.billing_config.clock_skew_tolerance_secs,
    )
    .map_err(|reason| {
        error_response(
            StatusCode::PAYMENT_REQUIRED,
            format!("invalid SpendAuth signature: {reason}"),
            "billing_error",
            "invalid_spend_auth",
        )
    })?;

    // Verify on-chain spending key
    match state.billing.get_account_info(&spend_auth.commitment).await {
        Ok(account_info) => {
            if recovered_address != account_info.spending_key {
                return Err(error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    "SpendAuth signer does not match account spending key".to_string(),
                    "billing_error",
                    "signer_mismatch",
                ));
            }

            let min_balance = state.billing_config.min_credit_balance;
            if min_balance > 0 && account_info.balance < alloy::primitives::U256::from(min_balance)
            {
                return Err(error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    format!(
                        "credit balance ({}) is below minimum required ({min_balance})",
                        account_info.balance
                    ),
                    "billing_error",
                    "insufficient_balance",
                ));
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to check account info");
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to verify account info".to_string(),
                "billing_error",
                "account_check_failed",
            ));
        }
    }

    Ok(requested_amount)
}

/// Prometheus metrics endpoint handler. Returns metrics in text format.
pub async fn metrics_handler() -> Response {
    let body = crate::metrics::gather();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Body::from(body))
        .unwrap_or_else(|e| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build metrics response: {e}"),
                "internal_error",
                "response_build_failed",
            )
        })
}

/// GPU health endpoint handler. Returns detected GPU info.
pub async fn gpu_health_handler() -> Result<Json<Vec<crate::health::GpuInfo>>, (StatusCode, String)>
{
    match crate::health::detect_gpus().await {
        Ok(gpus) => Ok(Json(gpus)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Try to acquire a semaphore permit from AppState. Returns 429 on failure.
#[allow(clippy::result_large_err)]
pub fn acquire_permit(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit, Response> {
    state.semaphore.clone().try_acquire_owned().map_err(|_| {
        error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "server at capacity".to_string(),
            "rate_limit_error",
            "too_many_requests",
        )
    })
}

/// Generic payment gate — works with any PaymentProvider.
///
/// Extracts payment proof from the request body (either `spend_auth` for
/// ShieldedCredits or `payment` for DirectTransfer), validates via the
/// provider's `authorize()`, returns the proof + authorized amount.
///
/// New blueprints should use this instead of `billing_gate`.
pub async fn payment_gate(
    provider: &dyn PaymentProvider,
    billing_config: &BillingConfig,
    headers: &HeaderMap,
    body_spend_auth: Option<SpendAuthPayload>,
    body_payment: Option<PaymentProof>,
    estimated_cost: u64,
) -> Result<(Option<PaymentProof>, Option<u64>), Response> {
    // Try to extract payment proof from multiple sources
    let proof = if let Some(p) = body_payment {
        Some(p)
    } else if let Some(sa) = body_spend_auth {
        Some(PaymentProof::SpendAuth(sa))
    } else if let Some(sa) = extract_x402_spend_auth(headers) {
        Some(PaymentProof::SpendAuth(sa))
    } else {
        None
    };

    if billing_config.billing_required && proof.is_none() {
        return Err(error_response(
            StatusCode::PAYMENT_REQUIRED,
            format!("payment required — estimated cost: {estimated_cost}"),
            "billing_error",
            "payment_required",
        ));
    }

    let Some(proof) = proof else {
        return Ok((None, None));
    };

    match provider.authorize(&proof).await {
        Ok(amount) => Ok((Some(proof), Some(amount))),
        Err(e) => Err(error_response(
            StatusCode::PAYMENT_REQUIRED,
            format!("payment authorization failed: {e}"),
            "billing_error",
            "authorization_failed",
        )),
    }
}

/// Settle payment after inference. Works with any PaymentProvider.
///
/// New blueprints should use this instead of `settle_billing`.
pub async fn settle_payment(
    provider: &dyn PaymentProvider,
    proof: &PaymentProof,
    actual_cost: u64,
) -> Result<(), anyhow::Error> {
    provider.settle(proof, actual_cost).await
}

/// Full billing gate: extract SpendAuth from body or x402 header, enforce
/// `billing_required`, validate signature/nonce/balance, and pre-authorize
/// the spend on-chain. Returns `(spend_auth, preauth_amount)` on success.
///
/// Callers settle after serving via [`settle_billing`].
pub async fn billing_gate(
    state: &AppState,
    headers: &HeaderMap,
    body_spend_auth: Option<SpendAuthPayload>,
    estimated_cost: u64,
) -> Result<(Option<SpendAuthPayload>, Option<u64>), Response> {
    let spend_auth = body_spend_auth.or_else(|| extract_x402_spend_auth(headers));

    if state.billing_config.billing_required && spend_auth.is_none() {
        return Err(payment_required(
            &state.billing_config,
            &state.tangle_config,
            state.operator_address,
            estimated_cost.max(state.billing_config.min_charge_amount),
        ));
    }

    let Some(spend_auth) = spend_auth else {
        return Ok((None, None));
    };

    let preauth_amount = validate_spend_auth(state, &spend_auth).await?;

    if let Err(e) = state.billing.authorize_spend(&spend_auth).await {
        tracing::error!(error = %e, "authorizeSpend failed");
        return Err(error_response(
            StatusCode::PAYMENT_REQUIRED,
            format!("billing authorization failed: {e}"),
            "billing_error",
            "authorization_failed",
        ));
    }

    Ok((Some(spend_auth), Some(preauth_amount)))
}

/// Settle billing after successful inference. Calculates the actual cost,
/// caps at pre-authorized amount, and claims payment on-chain.
///
/// If `claim_payment` fails after all retries and a recovery queue is
/// provided, the failed settlement is persisted for later retry.
///
/// Returns an error if `claim_payment` fails after all retries so callers
/// can log / alert appropriately.
pub async fn settle_billing(
    billing: &BillingClient,
    spend_auth: &SpendAuthPayload,
    preauth_amount: u64,
    actual_cost: u64,
) -> Result<(), anyhow::Error> {
    settle_billing_with_recovery(billing, spend_auth, preauth_amount, actual_cost, None).await
}

/// Like [`settle_billing`] but accepts an optional dead-letter queue for
/// failed settlements.
pub async fn settle_billing_with_recovery(
    billing: &BillingClient,
    spend_auth: &SpendAuthPayload,
    preauth_amount: u64,
    actual_cost: u64,
    recovery_queue: Option<&SettlementRecoveryQueue>,
) -> Result<(), anyhow::Error> {
    let charge_amount = actual_cost.min(preauth_amount);

    tracing::info!(
        actual_cost,
        preauth_amount,
        charge_amount,
        "settling billing (contract settles full pre-auth)"
    );

    if charge_amount > 0 {
        if let Err(e) = billing.claim_payment(spend_auth, charge_amount).await {
            if let Some(queue) = recovery_queue {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                queue.enqueue(FailedSettlement {
                    commitment: spend_auth.commitment.clone(),
                    nonce: spend_auth.nonce,
                    amount: charge_amount.to_string(),
                    operator: spend_auth.operator.clone(),
                    service_id: spend_auth.service_id,
                    timestamp: now,
                    error: format!("{e}"),
                    retry_count: 0,
                });
                tracing::error!(
                    error = %e,
                    "claimPayment failed — enqueued to dead-letter queue for retry"
                );
            }
            return Err(e);
        }
    }

    Ok(())
}
