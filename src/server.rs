use alloy::primitives::Address;
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::config::{BillingConfig, TangleConfig};
use crate::payment::{PaymentProof, PaymentProvider};
use crate::settlement_queue::{FailedSettlement, SettlementRecoveryQueue};

// x402 header constants
pub const X402_PAYMENT_REQUIRED: &str = "X-Payment-Required";
pub const X402_PAYMENT_TOKEN: &str = "X-Payment-Token";
pub const X402_PAYMENT_RECIPIENT: &str = "X-Payment-Recipient";
pub const X402_PAYMENT_NETWORK: &str = "X-Payment-Network";
pub const X402_PAYMENT_SIGNATURE: &str = "X-Payment-Signature";

pub use crate::state::{AccountGuard, AppState, AppStateBuilder, NonceKey, NonceStore};

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
#[allow(clippy::result_large_err)]
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
#[allow(clippy::result_large_err)]
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
    } else {
        extract_x402_spend_auth(headers).map(PaymentProof::SpendAuth)
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
#[allow(clippy::result_large_err)]
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

/// How a post-serve settlement should be executed on-chain. Factored out of
/// [`settle_billing_with_recovery`] so the dispatch decision is unit-testable
/// without a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementPlan {
    /// Usage metering available: call `settlePayment` with this amount. The
    /// operator receives exactly this and the contract refunds the unused
    /// pre-auth (`pre-auth − amount`) to the user's credit account.
    Metered(u64),
    /// Usage metering unavailable (e.g. streaming backend ended without a
    /// usage chunk): call `claimPayment`, settling the full pre-auth — the
    /// original settlement behavior.
    FullPreAuth(u64),
    /// Zero charge: nothing to settle on-chain.
    Skip,
}

/// Decide how to settle a served request. `actual_cost = Some(_)` is the
/// metered cost from real usage (capped at the pre-auth); `None` means usage
/// metering was unavailable and the full pre-auth is claimed.
pub fn plan_settlement(preauth_amount: u64, actual_cost: Option<u64>) -> SettlementPlan {
    match actual_cost {
        Some(cost) => {
            let charge = cost.min(preauth_amount);
            if charge == 0 {
                SettlementPlan::Skip
            } else {
                SettlementPlan::Metered(charge)
            }
        }
        None => {
            if preauth_amount == 0 {
                SettlementPlan::Skip
            } else {
                SettlementPlan::FullPreAuth(preauth_amount)
            }
        }
    }
}

/// Settle billing after successful inference.
///
/// When `actual_cost` is `Some`, the metered amount (capped at the pre-auth)
/// is settled via `settlePayment` and the contract refunds the unused pre-auth
/// to the user. When `actual_cost` is `None` — usage metering unavailable, see
/// [`plan_settlement`] — the full pre-auth is claimed via `claimPayment`,
/// unchanged from the original behavior.
///
/// If the on-chain settlement fails after all retries and a recovery queue is
/// provided, the failed settlement is persisted for later retry.
///
/// Returns an error if the settlement fails after all retries so callers
/// can log / alert appropriately.
pub async fn settle_billing(
    billing: &BillingClient,
    spend_auth: &SpendAuthPayload,
    preauth_amount: u64,
    actual_cost: Option<u64>,
) -> Result<(), anyhow::Error> {
    settle_billing_with_recovery(billing, spend_auth, preauth_amount, actual_cost, None).await
}

/// Like [`settle_billing`] but accepts an optional dead-letter queue for
/// failed settlements.
pub async fn settle_billing_with_recovery(
    billing: &BillingClient,
    spend_auth: &SpendAuthPayload,
    preauth_amount: u64,
    actual_cost: Option<u64>,
    recovery_queue: Option<&SettlementRecoveryQueue>,
) -> Result<(), anyhow::Error> {
    let plan = plan_settlement(preauth_amount, actual_cost);

    tracing::info!(actual_cost, preauth_amount, ?plan, "settling billing");

    let (charge_amount, result) = match plan {
        SettlementPlan::Metered(charge) => {
            (charge, billing.settle_payment(spend_auth, charge).await)
        }
        SettlementPlan::FullPreAuth(charge) => {
            (charge, billing.claim_payment(spend_auth, charge).await)
        }
        SettlementPlan::Skip => return Ok(()),
    };

    if let Err(e) = result {
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
                "settlement failed — enqueued to dead-letter queue for retry"
            );
        }
        return Err(e);
    }

    Ok(())
}

// ─── Unified, rail-agnostic request billing ───────────────────────────────────
//
// One reusable orchestration so a blueprint's request handler never re-implements
// per-rail billing: resolve a proof, authorize it (dispatched by rail), then
// settle it. Shielded and direct differ only inside these functions; the handler
// is identical for both.

/// A request's authorized payment, carried from the gate to settlement. Holds the
/// per-account RAII guard so the in-flight count stays balanced across serving.
pub struct AuthorizedPayment {
    pub proof: PaymentProof,
    /// Shielded pre-auth amount (the contract claims up to this). None for direct
    /// (already paid in full).
    pub preauth: Option<u64>,
    _account_guard: Option<AccountGuard>,
}

/// Resolve a request's payment proof, in priority order: an explicit
/// `DirectTransfer` payment, a body `spend_auth`, or an `X-Payment-Signature`
/// header. None when the caller sent no proof.
pub fn resolve_payment_proof(
    headers: &HeaderMap,
    body_spend_auth: Option<SpendAuthPayload>,
    body_payment: Option<PaymentProof>,
) -> Option<PaymentProof> {
    if let Some(p) = body_payment {
        Some(p)
    } else if let Some(sa) = body_spend_auth {
        Some(PaymentProof::SpendAuth(sa))
    } else {
        extract_x402_spend_auth(headers).map(PaymentProof::SpendAuth)
    }
}

/// Rail-agnostic pre-serve authorization. The order matters and is shared by all
/// rails: **per-account limit → validate → health → commit**, so a malformed or
/// over-limit request gets a precise 4xx (not a backend-health 503) and the
/// operator never commits gas / verifies payment for a backend it can't serve.
///   - **SpendAuth**: validate (sig / nonce / amount / operator) + pre-auth
///     ceiling (≤ 1.5× estimated, so a request can't lock up far more credit
///     than it can plausibly consume), then on-chain `authorizeSpend`.
///   - **DirectTransfer**: verify the on-chain transfer via the payment provider
///     (already paid; nothing to pre-authorize).
///
/// `is_healthy` is the backend readiness check, polled only after validation.
/// Returns the authorization to carry into [`settle_request`], or the 4xx
/// `Response` to hand back.
#[allow(clippy::result_large_err)]
pub async fn authorize_request(
    state: &AppState,
    proof: PaymentProof,
    estimated_max_cost: u64,
    is_healthy: impl std::future::Future<Output = bool> + Send,
) -> Result<AuthorizedPayment, Response> {
    // Per-account in-flight limit, keyed by payer identity (commitment / sender).
    // The guard releases the slot on any early return below.
    let max_per_account = state.server_config.max_per_account_requests;
    let account_guard = if max_per_account > 0 {
        match AccountGuard::acquire(
            proof.payer_id().to_string(),
            state.active_per_account.clone(),
            max_per_account,
        ) {
            Some(g) => Some(g),
            None => {
                return Err(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("account has too many active requests (limit: {max_per_account})"),
                    "rate_limit_error",
                    "per_account_limit",
                ));
            }
        }
    } else {
        None
    };

    // Validate FIRST (cheap, precise errors) — before the backend-health check.
    let preauth = match &proof {
        PaymentProof::SpendAuth(spend_auth) => {
            let preauth = validate_spend_auth(state, spend_auth).await?;
            // Reject an authorization that exceeds 1.5× the estimated max cost
            // so a request can't lock up far more credit than it can plausibly
            // consume (settlement itself is metered — the unused pre-auth is
            // refunded by `settlePayment`).
            let ceiling = estimated_max_cost.saturating_mul(3) / 2;
            if estimated_max_cost > 0 && preauth > ceiling {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "pre-auth amount ({preauth}) exceeds 1.5x estimated max cost \
                         ({estimated_max_cost}) — reduce amount to avoid locking excess credit"
                    ),
                    "billing_error",
                    "excessive_preauth",
                ));
            }
            Some(preauth)
        }
        PaymentProof::DirectTransfer { .. } => None,
    };

    // Don't commit gas / verify a payment for a backend we can't serve.
    if !is_healthy.await {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "inference backend is unavailable — payment not committed".to_string(),
            "upstream_error",
            "vllm_unhealthy",
        ));
    }

    // Commit: shielded authorizes on-chain; direct verifies the transfer.
    match &proof {
        PaymentProof::SpendAuth(spend_auth) => {
            if let Err(e) = state.billing.authorize_spend(spend_auth).await {
                tracing::error!(error = %e, "authorizeSpend failed");
                return Err(error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    format!("billing authorization failed: {e}"),
                    "billing_error",
                    "authorization_failed",
                ));
            }
        }
        PaymentProof::DirectTransfer { .. } => {
            if let Err(e) = state.payment_provider.authorize(&proof).await {
                return Err(error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    format!("direct payment verification failed: {e}"),
                    "billing_error",
                    "authorization_failed",
                ));
            }
        }
    }

    Ok(AuthorizedPayment {
        proof,
        preauth,
        _account_guard: account_guard,
    })
}

/// Settle a request's payment after serving, dispatched by rail. Shielded
/// settles the metered amount via `settlePayment` when `actual_cost` is known
/// (the contract refunds the unused pre-auth), or claims the full pre-auth via
/// `claimPayment` when usage metering is unavailable (e.g. a streaming backend
/// that ended without a usage chunk). Direct is already paid (no-op). Both go
/// through dead-letter recovery on failure. Errors are logged, not propagated —
/// a settlement failure must not fail the already-served response.
pub async fn settle_request(state: &AppState, auth: &AuthorizedPayment, actual_cost: Option<u64>) {
    if let PaymentProof::SpendAuth(ref spend_auth) = auth.proof {
        if let Some(preauth) = auth.preauth {
            if let Err(e) = settle_billing_with_recovery(
                &state.billing,
                spend_auth,
                preauth,
                actual_cost,
                state.settlement_recovery_queue.as_deref(),
            )
            .await
            {
                tracing::error!(error = %e, "settlement failed (enqueued for recovery if configured)");
            }
        }
    }
}
