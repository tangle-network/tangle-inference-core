//! End-to-end integration test for `tangle-inference-core`.
//!
//! Exercises the full path a blueprint takes when adopting the crate:
//! 1. Build `BillingConfig` + `TangleConfig` with test values.
//! 2. Create a `NonceStore` rooted in a temp dir.
//! 3. Construct an `AppState` via `AppStateBuilder` with a mock backend.
//! 4. Verify clone + cross-task usage.
//! 5. Test every concrete `CostModel` implementation.
//! 6. Test `SpendAuthPayload` signature recovery against a known keypair.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolValue;
use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature};

use axum::http::{HeaderMap, StatusCode};

use tangle_inference_core::billing::{recover_spend_auth_signer, verify_spend_auth_signature};
use tangle_inference_core::server::{
    error_response, extract_x402_spend_auth, payment_required, validate_spend_auth,
    X402_PAYMENT_NETWORK, X402_PAYMENT_RECIPIENT, X402_PAYMENT_REQUIRED, X402_PAYMENT_SIGNATURE,
    X402_PAYMENT_TOKEN,
};
use tangle_inference_core::{
    AppState, AppStateBuilder, BillingClient, BillingConfig, CostModel, CostParams,
    FlatRequestCostModel, NonceStore, PerCharCostModel, PerImageCostModel, PerSecondCostModel,
    PerTokenCostModel, ServerConfig, SpendAuthPayload, TangleConfig, TaskTypeCostModel,
};

/// Mock backend a blueprint would plug in. Any `Send + Sync + 'static` type works.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MockBackend {
    name: String,
    model: String,
}

fn test_tangle_config() -> TangleConfig {
    TangleConfig {
        // Anvil default account #0 private key.
        operator_key: "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".into(),
        rpc_url: "http://localhost:8545".into(),
        chain_id: 31337,
        shielded_credits: "0x5FbDB2315678afecb367f032d93F642f64180aa3".into(),
        blueprint_id: 1,
        service_id: Some(42),
    }
}

fn test_billing_config() -> BillingConfig {
    BillingConfig {
        payment_mode: tangle_inference_core::PaymentMode::Shielded,
        billing_required: true,
        max_spend_per_request: 1_000_000,
        min_credit_balance: 1_000,
        min_charge_amount: 100,
        claim_max_retries: 3,
        clock_skew_tolerance_secs: 30,
        max_gas_price_gwei: 0,
        nonce_store_path: None,
        direct_replay_store_path: None,
        payment_token_address: None,
    }
}

#[tokio::test]
async fn app_state_builder_constructs_and_clones_across_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let nonce_path = tmp.path().join("nonces.json");

    let billing =
        BillingClient::new(&test_tangle_config(), &test_billing_config()).expect("billing client");
    let operator = billing.operator_address();

    let state = AppStateBuilder::new()
        .billing(Arc::new(billing))
        .nonce_store(Arc::new(NonceStore::load(Some(nonce_path.clone()))))
        .server_config(Arc::new(ServerConfig {
            host: "127.0.0.1".into(),
            port: 8080,
            max_concurrent_requests: 8,
            max_request_body_bytes: 1024 * 1024,
            stream_timeout_secs: 60,
            idle_chunk_timeout_secs: 10,
            max_line_buf_bytes: 64 * 1024,
            max_per_account_requests: 0,
        }))
        .billing_config(Arc::new(test_billing_config()))
        .tangle_config(Arc::new(test_tangle_config()))
        .operator_address(operator)
        .max_concurrent(4)
        .backend(MockBackend {
            name: "mock".into(),
            model: "test-model".into(),
        })
        .build()
        .expect("build app state");

    // Backend downcast.
    let backend = state.backend::<MockBackend>().expect("downcast");
    assert_eq!(backend.name, "mock");
    assert_eq!(backend.model, "test-model");

    // Wrong type returns None.
    assert!(state.backend::<String>().is_none());

    // Clone + use across tasks.
    let s1 = state.clone();
    let s2 = state.clone();

    let h1 = tokio::spawn(async move {
        let b = s1.backend::<MockBackend>().unwrap();
        assert_eq!(b.name, "mock");
        s1.operator_address
    });
    let h2 = tokio::spawn(async move {
        assert_eq!(s2.semaphore.available_permits(), 4);
        s2.operator_address
    });

    let (a, b) = (h1.await.unwrap(), h2.await.unwrap());
    assert_eq!(a, b);
    assert_eq!(a, operator);
}

#[tokio::test]
async fn app_state_builder_rejects_missing_required_fields() {
    let result = AppStateBuilder::new().build();
    let err = match result {
        Ok(_) => panic!("expected builder to reject empty config"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("billing"));
}

#[test]
fn per_token_cost_model() {
    let m = PerTokenCostModel {
        price_per_input_token: 2,
        price_per_output_token: 5,
    };
    let cost = m.calculate_cost(&CostParams {
        prompt_tokens: 100,
        completion_tokens: 50,
        ..Default::default()
    });
    assert_eq!(cost, 100 * 2 + 50 * 5);
}

#[test]
fn per_char_cost_model() {
    let m = PerCharCostModel {
        price_per_1k_chars: 4_000,
    };
    let mut extra = HashMap::new();
    extra.insert("characters".into(), 2_500);
    let cost = m.calculate_cost(&CostParams {
        extra,
        ..Default::default()
    });
    // 2500 * 4000 / 1000 = 10_000
    assert_eq!(cost, 10_000);

    // Missing characters key -> 0.
    assert_eq!(m.calculate_cost(&CostParams::default()), 0);
}

#[test]
fn per_second_cost_model() {
    let m = PerSecondCostModel {
        price_per_second: 1_000,
    };
    let mut extra = HashMap::new();
    // 250 centiseconds = 2.5 seconds.
    extra.insert("centiseconds".into(), 250);
    let cost = m.calculate_cost(&CostParams {
        extra,
        ..Default::default()
    });
    // 250 * 1000 / 100 = 2_500
    assert_eq!(cost, 2_500);
}

#[test]
fn per_image_cost_model() {
    let m = PerImageCostModel {
        price_per_image: 7_500,
    };
    let mut extra = HashMap::new();
    extra.insert("images".into(), 3);
    let cost = m.calculate_cost(&CostParams {
        extra,
        ..Default::default()
    });
    assert_eq!(cost, 22_500);

    // Defaults to 1 image when key missing.
    assert_eq!(m.calculate_cost(&CostParams::default()), 7_500);
}

#[test]
fn flat_request_cost_model() {
    let m = FlatRequestCostModel {
        price_per_request: 999,
    };
    assert_eq!(m.calculate_cost(&CostParams::default()), 999);
    assert_eq!(
        m.calculate_cost(&CostParams {
            prompt_tokens: 9999,
            completion_tokens: 9999,
            ..Default::default()
        }),
        999
    );
}

#[test]
fn task_type_cost_model_dispatches_by_task() {
    let mut per_task: HashMap<String, Box<dyn CostModel>> = HashMap::new();
    per_task.insert(
        "tts".into(),
        Box::new(PerCharCostModel {
            price_per_1k_chars: 4_000,
        }),
    );
    per_task.insert(
        "image".into(),
        Box::new(PerImageCostModel {
            price_per_image: 50_000,
        }),
    );

    let model = TaskTypeCostModel {
        default: Box::new(PerTokenCostModel {
            price_per_input_token: 1,
            price_per_output_token: 2,
        }),
        per_task,
    };

    // tts task -> per-char.
    let mut tts_extra = HashMap::new();
    tts_extra.insert("characters".into(), 1_000);
    let tts_cost = model.calculate_cost(&CostParams {
        task_type: Some("tts".into()),
        extra: tts_extra,
        ..Default::default()
    });
    assert_eq!(tts_cost, 4_000);

    // image task -> per-image.
    let mut img_extra = HashMap::new();
    img_extra.insert("images".into(), 2);
    let img_cost = model.calculate_cost(&CostParams {
        task_type: Some("image".into()),
        extra: img_extra,
        ..Default::default()
    });
    assert_eq!(img_cost, 100_000);

    // Unknown task -> default per-token.
    let chat_cost = model.calculate_cost(&CostParams {
        task_type: Some("chat".into()),
        prompt_tokens: 10,
        completion_tokens: 20,
        ..Default::default()
    });
    assert_eq!(chat_cost, 10 + 40);

    // No task -> default.
    let no_task = model.calculate_cost(&CostParams {
        prompt_tokens: 5,
        completion_tokens: 0,
        ..Default::default()
    });
    assert_eq!(no_task, 5);
}

#[test]
fn spend_auth_signature_round_trip() {
    // Anvil account #0.
    let key_hex = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let signer: PrivateKeySigner = key_hex.parse().expect("signer");
    let signing_address = signer.address();

    let shielded_credits_addr = "0x5FbDB2315678afecb367f032d93F642f64180aa3";
    let shielded: Address = shielded_credits_addr.parse().unwrap();
    let chain_id: u64 = 31337;

    // Build the EIP-712 digest exactly as `recover_spend_auth_signer` does.
    let domain_separator = keccak256(
        (
            keccak256(
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            ),
            keccak256(b"ShieldedCredits"),
            keccak256(b"1"),
            U256::from(chain_id),
            shielded,
        )
            .abi_encode(),
    );
    let spend_typehash = keccak256(
        b"SpendAuthorization(bytes32 commitment,uint64 serviceId,uint8 jobIndex,uint256 amount,address operator,uint256 nonce,uint64 expiry)",
    );

    let commitment_str = "0x1111111111111111111111111111111111111111111111111111111111111111";
    let commitment: B256 = commitment_str.parse().unwrap();
    let service_id: u64 = 42;
    let job_index: u8 = 0;
    let amount_u: U256 = U256::from(500_000u64);
    let operator: Address = "0x000000000000000000000000000000000000dEaD"
        .parse()
        .unwrap();
    let nonce: u64 = 7;
    let expiry: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    let struct_hash = keccak256(
        (
            spend_typehash,
            commitment,
            U256::from(service_id),
            U256::from(job_index),
            amount_u,
            operator,
            U256::from(nonce),
            U256::from(expiry),
        )
            .abi_encode(),
    );

    let digest = keccak256(
        [
            &[0x19, 0x01],
            domain_separator.as_slice(),
            struct_hash.as_slice(),
        ]
        .concat(),
    );

    // Sign with the underlying k256 signing key.
    let key_bytes = hex::decode(key_hex).unwrap();
    let signing_key = k256::ecdsa::SigningKey::from_slice(&key_bytes).unwrap();
    let (sig, recid): (Signature, RecoveryId) =
        signing_key.sign_prehash(digest.as_slice()).unwrap();

    let mut sig_bytes = [0u8; 65];
    sig_bytes[..64].copy_from_slice(&sig.to_bytes());
    sig_bytes[64] = recid.to_byte();
    let signature_hex = format!("0x{}", hex::encode(sig_bytes));

    let payload = SpendAuthPayload {
        commitment: commitment_str.into(),
        service_id,
        job_index,
        amount: "500000".into(),
        operator: format!("{operator:#x}"),
        nonce,
        expiry,
        signature: signature_hex,
    };

    // Recover and verify.
    let recovered =
        recover_spend_auth_signer(&payload, shielded_credits_addr, chain_id, 30).unwrap();
    assert_eq!(recovered, signing_address);

    verify_spend_auth_signature(
        &payload,
        signing_address,
        shielded_credits_addr,
        chain_id,
        30,
    )
    .expect("matching signer");

    // Mismatched expected key -> Err.
    let other: Address = "0x000000000000000000000000000000000000bEEF"
        .parse()
        .unwrap();
    assert!(
        verify_spend_auth_signature(&payload, other, shielded_credits_addr, chain_id, 30).is_err()
    );
}

#[tokio::test]
async fn nonce_store_persists_across_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonces.json");

    let store = NonceStore::load(Some(path.clone()));
    let key = ("0xabc".to_string(), 1u64);
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;

    assert!(!store.check_replay(&key, 30).await);
    store.insert(key.clone(), expiry, 30).await;
    assert!(store.check_replay(&key, 30).await);

    // Reload from disk.
    let reloaded = NonceStore::load(Some(path));
    assert!(reloaded.check_replay(&key, 30).await);
}

// --- Helpers for validate_spend_auth / payment_required / extract_x402 tests ---

fn build_test_state() -> AppState {
    let billing =
        BillingClient::new(&test_tangle_config(), &test_billing_config()).expect("billing client");
    let operator = billing.operator_address();
    let tmp = tempfile::tempdir().unwrap();
    let nonce_path = tmp.path().join("nonces.json");

    // Leak the tempdir so it lives long enough for the test.
    std::mem::forget(tmp);

    AppStateBuilder::new()
        .billing(Arc::new(billing))
        .nonce_store(Arc::new(NonceStore::load(Some(nonce_path))))
        .server_config(Arc::new(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            max_concurrent_requests: 8,
            max_request_body_bytes: 1024 * 1024,
            stream_timeout_secs: 60,
            idle_chunk_timeout_secs: 10,
            max_line_buf_bytes: 64 * 1024,
            max_per_account_requests: 0,
        }))
        .billing_config(Arc::new(test_billing_config()))
        .tangle_config(Arc::new(test_tangle_config()))
        .operator_address(operator)
        .max_concurrent(4)
        .backend(MockBackend {
            name: "test".into(),
            model: "test".into(),
        })
        .build()
        .expect("build state")
}

fn make_spend_auth(operator: Address, amount: u64, nonce: u64) -> SpendAuthPayload {
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    SpendAuthPayload {
        commitment: "0x1111111111111111111111111111111111111111111111111111111111111111".into(),
        service_id: 42,
        job_index: 0,
        amount: amount.to_string(),
        operator: format!("{operator:#x}"),
        nonce,
        expiry,
        signature: "0x".to_string() + &"00".repeat(65), // dummy sig
    }
}

fn response_status(resp: &axum::response::Response) -> StatusCode {
    resp.status()
}

// --- validate_spend_auth tests ---

#[tokio::test]
async fn validate_spend_auth_rejects_amount_below_min_charge() {
    let state = build_test_state();
    // min_charge_amount is 100, so 50 should fail.
    let auth = make_spend_auth(state.operator_address, 50, 100);
    let result = validate_spend_auth(&state, &auth).await;
    assert!(result.is_err());
    let resp = result.unwrap_err();
    assert_eq!(response_status(&resp), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_spend_auth_rejects_amount_above_max_spend() {
    let state = build_test_state();
    // max_spend_per_request is 1_000_000, so 2_000_000 should fail.
    let auth = make_spend_auth(state.operator_address, 2_000_000, 101);
    let result = validate_spend_auth(&state, &auth).await;
    assert!(result.is_err());
    let resp = result.unwrap_err();
    assert_eq!(response_status(&resp), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_spend_auth_rejects_wrong_operator() {
    let state = build_test_state();
    let wrong_operator: Address = "0x000000000000000000000000000000000000dEaD"
        .parse()
        .unwrap();
    let auth = make_spend_auth(wrong_operator, 500, 102);
    let result = validate_spend_auth(&state, &auth).await;
    assert!(result.is_err());
    let resp = result.unwrap_err();
    assert_eq!(response_status(&resp), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_spend_auth_rejects_expired_nonce() {
    let state = build_test_state();
    let mut auth = make_spend_auth(state.operator_address, 500, 103);
    // Set expiry in the past (well beyond clock_skew_tolerance of 30s).
    auth.expiry = 1;
    let result = validate_spend_auth(&state, &auth).await;
    // The nonce check itself won't reject an expired nonce (it's evicted),
    // but the signature recovery step will reject it due to expiry check.
    // Either way, should be Err.
    assert!(result.is_err());
}

#[tokio::test]
async fn validate_spend_auth_rejects_nonce_replay() {
    let state = build_test_state();
    let auth = make_spend_auth(state.operator_address, 500, 200);

    // First call — passes amount/operator/nonce checks, fails later at
    // signature recovery (no real sig). That's fine: the nonce is now consumed.
    let _ = validate_spend_auth(&state, &auth).await;

    // Second call with same nonce — should be rejected as replay BEFORE
    // hitting signature recovery. This tests the TOCTOU fix.
    let result = validate_spend_auth(&state, &auth).await;
    assert!(result.is_err());
    let resp = result.unwrap_err();
    assert_eq!(response_status(&resp), StatusCode::BAD_REQUEST);
}

// --- payment_required tests ---

#[tokio::test]
async fn payment_required_returns_402_with_x402_headers() {
    let billing_cfg = test_billing_config();
    let tangle_cfg = test_tangle_config();
    let operator: Address = "0x000000000000000000000000000000000000dEaD"
        .parse()
        .unwrap();

    let resp = payment_required(&billing_cfg, &tangle_cfg, operator, 5000);

    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    let headers = resp.headers();
    assert!(headers.contains_key(X402_PAYMENT_REQUIRED));
    assert!(headers.contains_key(X402_PAYMENT_TOKEN));
    assert!(headers.contains_key(X402_PAYMENT_RECIPIENT));
    assert!(headers.contains_key(X402_PAYMENT_NETWORK));

    // Amount should be max(estimated, min_charge).
    let amount_header = headers[X402_PAYMENT_REQUIRED].to_str().unwrap();
    let amount: u64 = amount_header.parse().unwrap();
    assert!(amount >= billing_cfg.min_charge_amount);
    assert_eq!(amount, 5000); // 5000 > min_charge(100)
}

#[tokio::test]
async fn payment_required_enforces_min_charge() {
    let billing_cfg = test_billing_config();
    let tangle_cfg = test_tangle_config();
    let operator: Address = "0x000000000000000000000000000000000000dEaD"
        .parse()
        .unwrap();

    // Request amount below min_charge — should be bumped.
    let resp = payment_required(&billing_cfg, &tangle_cfg, operator, 10);
    let amount_header = resp.headers()[X402_PAYMENT_REQUIRED].to_str().unwrap();
    let amount: u64 = amount_header.parse().unwrap();
    assert_eq!(amount, billing_cfg.min_charge_amount);
}

// --- extract_x402_spend_auth tests ---

#[test]
fn extract_x402_spend_auth_parses_valid_json_header() {
    let payload = SpendAuthPayload {
        commitment: "0xabc".into(),
        service_id: 1,
        job_index: 0,
        amount: "1000".into(),
        operator: "0x000000000000000000000000000000000000dEaD".into(),
        nonce: 42,
        expiry: 9999999999,
        signature: "0xdeadbeef".into(),
    };
    let json = serde_json::to_string(&payload).unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(X402_PAYMENT_SIGNATURE, json.parse().unwrap());

    let result = extract_x402_spend_auth(&headers);
    assert!(result.is_some());
    let parsed = result.unwrap();
    assert_eq!(parsed.nonce, 42);
    assert_eq!(parsed.commitment, "0xabc");
}

#[test]
fn extract_x402_spend_auth_returns_none_for_missing_header() {
    let headers = HeaderMap::new();
    assert!(extract_x402_spend_auth(&headers).is_none());
}

#[test]
fn extract_x402_spend_auth_returns_none_for_invalid_header() {
    let mut headers = HeaderMap::new();
    headers.insert(X402_PAYMENT_SIGNATURE, "not-json".parse().unwrap());
    assert!(extract_x402_spend_auth(&headers).is_none());
}

// --- error_response tests ---

#[tokio::test]
async fn error_response_returns_correct_status_and_json() {
    let resp = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate limited".into(),
        "rate_limit",
        "too_many_requests",
    );
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["message"], "rate limited");
    assert_eq!(json["error"]["type"], "rate_limit");
    assert_eq!(json["error"]["code"], "too_many_requests");
}

#[tokio::test]
async fn check_and_insert_is_atomic_under_contention() {
    let store = Arc::new(NonceStore::load(None));
    let key = ("commitment-race".to_string(), 42u64);
    let expiry = 9999999999u64;
    let tolerance = 300u64;

    let mut handles = Vec::new();
    for _ in 0..100 {
        let store = store.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            store.check_and_insert(key, expiry, tolerance).await
        }));
    }

    let results: Vec<bool> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let fresh_count = results.iter().filter(|&&was_replay| !was_replay).count();
    let replay_count = results.iter().filter(|&&was_replay| was_replay).count();

    assert_eq!(fresh_count, 1, "exactly one task should see a fresh nonce");
    assert_eq!(replay_count, 99, "all others should see replay");
}

#[tokio::test]
async fn app_state_from_config_constructs_correctly() {
    let tangle = test_tangle_config();
    let server = ServerConfig {
        host: "127.0.0.1".into(),
        port: 8080,
        max_concurrent_requests: 8,
        max_request_body_bytes: 1024 * 1024,
        stream_timeout_secs: 60,
        idle_chunk_timeout_secs: 10,
        max_line_buf_bytes: 64 * 1024,
        max_per_account_requests: 0,
    };
    let billing = test_billing_config();

    let state = AppState::from_config(
        &tangle,
        &server,
        &billing,
        4,
        MockBackend {
            name: "from-config".into(),
            model: "test".into(),
        },
    )
    .expect("from_config should succeed");

    let backend = state.backend::<MockBackend>().expect("downcast");
    assert_eq!(backend.name, "from-config");
    assert_eq!(state.semaphore.available_permits(), 4);
}

// ─── Payment provider adversarial tests ──────────────────────────────
//
// These test the PaymentProvider trait + PaymentProof type for:
// - Type confusion (wrong proof variant for wrong provider)
// - Malformed inputs (empty strings, huge amounts, garbage tx hashes)
// - NoopProvider never rejects
// - ShieldedProvider rejects DirectTransfer proofs
// - DirectProvider rejects SpendAuth proofs
// - PaymentMode config creates the right provider
// - PaymentProof serde round-trip (callers send JSON, we deserialize)
// - create_provider factory with each mode

use tangle_inference_core::payment::{
    create_provider, NoopProvider, PaymentMode, PaymentProof, PaymentProvider, ShieldedProvider,
};

/// Anvil default account #0 private key.
const TEST_OPERATOR_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

fn test_operator_signer() -> PrivateKeySigner {
    TEST_OPERATOR_KEY.parse().unwrap()
}

fn test_spend_auth_payload() -> SpendAuthPayload {
    SpendAuthPayload {
        commitment: "0x0000000000000000000000000000000000000000000000000000000000000001".into(),
        service_id: 42,
        job_index: 0,
        amount: "100000".into(),
        operator: format!("{}", test_operator_signer().address()),
        nonce: 1,
        expiry: u64::MAX,                               // far future
        signature: "0x".to_string() + &"00".repeat(65), // dummy sig — won't verify on-chain but sufficient for type tests
    }
}

#[tokio::test]
async fn noop_provider_always_authorizes() {
    let provider = NoopProvider::new(TEST_OPERATOR_KEY.to_string()).unwrap();
    // Even garbage proof gets authorized
    let proof = PaymentProof::DirectTransfer {
        tx_hash: "0xgarbage".into(),
        from: "0xdead".into(),
        amount: "999999999".into(),
        token: "".into(),
    };
    let result = provider.authorize(&proof).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);
}

#[tokio::test]
async fn noop_provider_settle_is_noop() {
    let provider = NoopProvider::new(TEST_OPERATOR_KEY.to_string()).unwrap();
    let proof = PaymentProof::SpendAuth(test_spend_auth_payload());
    let result = provider.settle(&proof, 12345).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn shielded_provider_rejects_direct_transfer_proof() {
    let tangle = test_tangle_config();
    let billing = test_billing_config();
    let provider = ShieldedProvider::new(&tangle, &billing).unwrap();
    let proof = PaymentProof::DirectTransfer {
        tx_hash: "0xabc".into(),
        from: "0xdead".into(),
        amount: "100".into(),
        token: "0x0".into(),
    };
    let result = provider.authorize(&proof).await;
    assert!(
        result.is_err(),
        "ShieldedProvider must reject DirectTransfer proof"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("SpendAuth"),
        "error should mention SpendAuth, got: {err}"
    );
}

#[tokio::test]
async fn shielded_provider_rejects_direct_transfer_on_settle() {
    let tangle = test_tangle_config();
    let billing = test_billing_config();
    let provider = ShieldedProvider::new(&tangle, &billing).unwrap();
    let proof = PaymentProof::DirectTransfer {
        tx_hash: "0xabc".into(),
        from: "0x0".into(),
        amount: "0".into(),
        token: "".into(),
    };
    let result = provider.settle(&proof, 0).await;
    assert!(result.is_err());
}

#[test]
fn payment_proof_serde_round_trip_spend_auth() {
    let payload = test_spend_auth_payload();
    let proof = PaymentProof::SpendAuth(payload);
    let json = serde_json::to_string(&proof).unwrap();
    assert!(
        json.contains("\"type\":\"spend_auth\""),
        "tag should be spend_auth"
    );
    let parsed: PaymentProof = serde_json::from_str(&json).unwrap();
    match parsed {
        PaymentProof::SpendAuth(p) => assert_eq!(
            p.commitment,
            "0x0000000000000000000000000000000000000000000000000000000000000001"
        ),
        _ => panic!("expected SpendAuth variant"),
    }
}

#[test]
fn payment_proof_serde_round_trip_direct_transfer() {
    let proof = PaymentProof::DirectTransfer {
        tx_hash: "0xdeadbeef".into(),
        from: "0x1234".into(),
        amount: "1000000".into(),
        token: "0xtoken".into(),
    };
    let json = serde_json::to_string(&proof).unwrap();
    assert!(json.contains("\"type\":\"direct_transfer\""));
    let parsed: PaymentProof = serde_json::from_str(&json).unwrap();
    match parsed {
        PaymentProof::DirectTransfer {
            tx_hash, amount, ..
        } => {
            assert_eq!(tx_hash, "0xdeadbeef");
            assert_eq!(amount, "1000000");
        }
        _ => panic!("expected DirectTransfer variant"),
    }
}

#[test]
fn payment_proof_rejects_unknown_type_tag() {
    let json = r#"{"type":"bitcoin","tx_hash":"abc"}"#;
    let result = serde_json::from_str::<PaymentProof>(json);
    assert!(
        result.is_err(),
        "unknown payment type should fail deserialization"
    );
}

#[test]
fn payment_proof_rejects_missing_type_tag() {
    let json = r#"{"tx_hash":"0xabc","amount":"100"}"#;
    let result = serde_json::from_str::<PaymentProof>(json);
    assert!(
        result.is_err(),
        "missing type tag should fail deserialization"
    );
}

#[test]
fn create_provider_noop_mode() {
    let tangle = test_tangle_config();
    let billing = test_billing_config();
    let provider = create_provider(PaymentMode::None, &tangle, &billing);
    assert!(provider.is_ok());
}

#[test]
fn create_provider_shielded_mode() {
    let tangle = test_tangle_config();
    let billing = test_billing_config();
    let provider = create_provider(PaymentMode::Shielded, &tangle, &billing);
    assert!(provider.is_ok());
}

#[test]
fn create_provider_direct_mode_requires_token() {
    let tangle = test_tangle_config();
    let billing = test_billing_config(); // no payment_token_address
    let provider = create_provider(PaymentMode::Direct, &tangle, &billing);
    assert!(provider.is_err(), "direct mode without token must fail");

    let mut billing_with_token = test_billing_config();
    billing_with_token.payment_token_address =
        Some("0x0000000000000000000000000000000000000001".into());
    let provider = create_provider(PaymentMode::Direct, &tangle, &billing_with_token);
    assert!(provider.is_ok());
}

#[test]
fn payment_mode_default_is_shielded() {
    let mode: PaymentMode = serde_json::from_str(r#""shielded""#).unwrap();
    assert_eq!(mode, PaymentMode::Shielded);
    assert_eq!(PaymentMode::default(), PaymentMode::Shielded);
}

#[test]
fn payment_mode_serde_all_variants() {
    for (json, expected) in [
        (r#""none""#, PaymentMode::None),
        (r#""shielded""#, PaymentMode::Shielded),
        (r#""direct""#, PaymentMode::Direct),
    ] {
        let parsed: PaymentMode = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, expected);
        let back = serde_json::to_string(&parsed).unwrap();
        assert_eq!(back, json);
    }
}

#[test]
fn noop_provider_operator_address_matches_key() {
    let provider = NoopProvider::new(TEST_OPERATOR_KEY.to_string()).unwrap();
    let expected: Address = test_operator_signer().address();
    assert_eq!(provider.operator_address(), expected);
}

#[test]
fn noop_provider_rejects_invalid_key() {
    let result = NoopProvider::new("not-a-valid-hex-key".into());
    assert!(result.is_err());
}

// ─── Real Anvil E2E: DirectProvider with real ERC-20 transfer ─────────
//
// Spawns Anvil, deploys a minimal ERC-20, mints tokens, does a real transfer
// to the operator address, then verifies DirectProvider.authorize() accepts
// the receipt. Zero mocks — real EVM execution.
//
// Gate: ANVIL_E2E=1 (skip in CI unless foundry is installed)

/// Minimal ERC-20 that lets us mint and transfer.
/// Compiled from: constructor sets deployer as minter, mint(to, amount), transfer works via OZ.
/// For test simplicity: we use alloy's sol! macro to deploy inline.
mod anvil_e2e {
    use super::*;
    use alloy::network::EthereumWallet;
    use alloy::providers::ProviderBuilder;
    use alloy::sol;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;
    use tangle_inference_core::payment::{DirectProvider, PaymentProof, PaymentProvider};

    // Minimal ERC-20 compiled from tests/TestERC20.sol with solc --optimize
    sol! {
        #[sol(rpc, bytecode = "6080604052348015600e575f80fd5b506102da8061001c5f395ff3fe608060405234801561000f575f80fd5b506004361061003f575f3560e01c806340c10f191461004357806370a0823114610058578063a9059cbb1461008a575b5f80fd5b610056610051366004610222565b6100ad565b005b61007761006636600461024a565b5f6020819052908152604090205481565b6040519081526020015b60405180910390f35b61009d610098366004610222565b61011d565b6040519015158152602001610081565b6001600160a01b0382165f90815260208190526040812080548392906100d490849061027e565b90915550506040518181526001600160a01b038316905f907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a35050565b335f9081526020819052604081205482111561016e5760405162461bcd60e51b815260206004820152600c60248201526b1a5b9cdd59999a58da595b9d60a21b604482015260640160405180910390fd5b335f908152602081905260408120805484929061018c908490610291565b90915550506001600160a01b0383165f90815260208190526040812080548492906101b890849061027e565b90915550506040518281526001600160a01b0384169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a35060015b92915050565b80356001600160a01b038116811461021d575f80fd5b919050565b5f8060408385031215610233575f80fd5b61023c83610207565b946020939093013593505050565b5f6020828403121561025a575f80fd5b61026382610207565b9392505050565b634e487b7160e01b5f52601160045260245ffd5b808201808211156102015761020161026a565b818103818111156102015761020161026a56fea2646970667358221220001234497cefa9f349ddcdf662ebeb0ca10ba368b3f04c0bbfa52d9fa39db83d64736f6c634300081a0033")]
        contract TestToken {
            function mint(address to, uint256 amount) external;
            function transfer(address to, uint256 amount) external returns (bool);
            function balanceOf(address owner) external view returns (uint256);
        }
    }

    struct AnvilInstance {
        child: Child,
        port: u16,
    }

    impl AnvilInstance {
        fn spawn() -> Self {
            // Use port 0 → Anvil picks a free port. We parse it from stdout.
            // But Anvil --silent suppresses that, so we bind a TcpListener
            // to port 0 first, get the port, close it, then give it to Anvil.
            let port = {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.local_addr().unwrap().port()
            };
            let child = Command::new("anvil")
                .args(["--port", &port.to_string(), "--silent"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("anvil must be installed (foundry)");

            // Wait for it to be ready
            std::thread::sleep(Duration::from_millis(800));
            AnvilInstance { child, port }
        }

        fn rpc_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for AnvilInstance {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Anvil default account #1 (the "caller" who pays)
    const CALLER_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    #[tokio::test]
    async fn direct_provider_authorizes_real_erc20_transfer() {
        if std::env::var("ANVIL_E2E").unwrap_or_default() != "1" {
            eprintln!("skipping anvil e2e (set ANVIL_E2E=1)");
            return;
        }

        let anvil = AnvilInstance::spawn();
        let rpc = anvil.rpc_url();

        // Deployer = Anvil account #0 (same as operator key)
        let deployer_signer: PrivateKeySigner = TEST_OPERATOR_KEY.parse().unwrap();
        let deployer_wallet = EthereumWallet::from(deployer_signer.clone());
        let deployer_provider = ProviderBuilder::new()
            .wallet(deployer_wallet)
            .connect_http(rpc.parse().unwrap());

        // Deploy test ERC-20
        let token = TestToken::deploy(&deployer_provider).await.unwrap();
        let token_addr = *token.address();

        // Mint 1M tokens to the caller (account #1)
        let caller_signer: PrivateKeySigner = CALLER_KEY.parse().unwrap();
        let caller_addr = caller_signer.address();
        let mint_amount = U256::from(1_000_000u64);
        token
            .mint(caller_addr, mint_amount)
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        // Verify balance
        let bal = token.balanceOf(caller_addr).call().await.unwrap();
        assert_eq!(bal, mint_amount);

        // Caller transfers 500K to operator
        let caller_wallet = EthereumWallet::from(caller_signer);
        let caller_provider = ProviderBuilder::new()
            .wallet(caller_wallet)
            .connect_http(rpc.parse().unwrap());
        let caller_token = TestToken::new(token_addr, &caller_provider);

        let transfer_amount = U256::from(500_000u64);
        let operator_addr = deployer_signer.address();
        let tx_receipt = caller_token
            .transfer(operator_addr, transfer_amount)
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        let tx_hash = format!("{:#x}", tx_receipt.transaction_hash);

        // Now: DirectProvider should authorize this transfer
        let provider = DirectProvider::new(
            rpc.clone(),
            TEST_OPERATOR_KEY.into(),
            Some(format!("{:#x}", token_addr)),
            0, // 0 confirmations for test (anvil auto-mines),
            None, // replay store (in-memory for test)
        )
        .unwrap();

        let proof = PaymentProof::DirectTransfer {
            tx_hash: tx_hash.clone(),
            from: format!("{:#x}", caller_addr),
            amount: "500000".into(),
            token: format!("{:#x}", token_addr),
        };

        let authorized = provider.authorize(&proof).await;
        assert!(
            authorized.is_ok(),
            "authorize failed: {:?}",
            authorized.err()
        );
        assert_eq!(authorized.unwrap(), 500_000);

        // Settle should be a no-op
        let settle = provider.settle(&proof, 400_000).await;
        assert!(settle.is_ok());
    }

    #[tokio::test]
    async fn direct_provider_rejects_insufficient_transfer_amount() {
        if std::env::var("ANVIL_E2E").unwrap_or_default() != "1" {
            eprintln!("skipping anvil e2e (set ANVIL_E2E=1)");
            return;
        }

        let anvil = AnvilInstance::spawn();
        let rpc = anvil.rpc_url();

        let deployer_signer: PrivateKeySigner = TEST_OPERATOR_KEY.parse().unwrap();
        let deployer_wallet = EthereumWallet::from(deployer_signer.clone());
        let deployer_provider = ProviderBuilder::new()
            .wallet(deployer_wallet)
            .connect_http(rpc.parse().unwrap());

        let token = TestToken::deploy(&deployer_provider).await.unwrap();
        let token_addr = *token.address();

        let caller_signer: PrivateKeySigner = CALLER_KEY.parse().unwrap();
        let caller_addr = caller_signer.address();
        token
            .mint(caller_addr, U256::from(1000u64))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        let caller_wallet = EthereumWallet::from(caller_signer);
        let caller_provider = ProviderBuilder::new()
            .wallet(caller_wallet)
            .connect_http(rpc.parse().unwrap());
        let caller_token = TestToken::new(token_addr, &caller_provider);

        // Transfer only 100 tokens
        let tx_receipt = caller_token
            .transfer(deployer_signer.address(), U256::from(100u64))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        let provider = DirectProvider::new(
            rpc,
            TEST_OPERATOR_KEY.into(),
            Some(format!("{:#x}", token_addr)),
            0,
            None, // replay store (in-memory for test)
        )
        .unwrap();

        // Claim 500 but only transferred 100 — should reject
        let proof = PaymentProof::DirectTransfer {
            tx_hash: format!("{:#x}", tx_receipt.transaction_hash),
            from: format!("{:#x}", caller_addr),
            amount: "500".into(),
            token: format!("{:#x}", token_addr),
        };

        let result = provider.authorize(&proof).await;
        assert!(result.is_err(), "should reject insufficient amount");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("less than requested"));
    }

    #[tokio::test]
    async fn direct_provider_rejects_transfer_to_wrong_recipient() {
        if std::env::var("ANVIL_E2E").unwrap_or_default() != "1" {
            eprintln!("skipping anvil e2e (set ANVIL_E2E=1)");
            return;
        }

        let anvil = AnvilInstance::spawn();
        let rpc = anvil.rpc_url();

        let deployer_signer: PrivateKeySigner = TEST_OPERATOR_KEY.parse().unwrap();
        let deployer_wallet = EthereumWallet::from(deployer_signer.clone());
        let deployer_provider = ProviderBuilder::new()
            .wallet(deployer_wallet)
            .connect_http(rpc.parse().unwrap());

        let token = TestToken::deploy(&deployer_provider).await.unwrap();
        let token_addr = *token.address();

        let caller_signer: PrivateKeySigner = CALLER_KEY.parse().unwrap();
        let caller_addr = caller_signer.address();
        token
            .mint(caller_addr, U256::from(1000u64))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        // Transfer to a RANDOM address, not the operator
        let random_recipient: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();
        let caller_wallet = EthereumWallet::from(caller_signer);
        let caller_provider = ProviderBuilder::new()
            .wallet(caller_wallet)
            .connect_http(rpc.parse().unwrap());
        let caller_token = TestToken::new(token_addr, &caller_provider);

        let tx_receipt = caller_token
            .transfer(random_recipient, U256::from(500u64))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        // Operator's DirectProvider should reject — transfer wasn't to them
        let provider = DirectProvider::new(
            rpc,
            TEST_OPERATOR_KEY.into(),
            Some(format!("{:#x}", token_addr)),
            0,
            None, // replay store (in-memory for test)
        )
        .unwrap();

        let proof = PaymentProof::DirectTransfer {
            tx_hash: format!("{:#x}", tx_receipt.transaction_hash),
            from: format!("{:#x}", caller_addr),
            amount: "500".into(),
            token: format!("{:#x}", token_addr),
        };

        let result = provider.authorize(&proof).await;
        assert!(result.is_err(), "should reject transfer to wrong recipient");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no ERC-20 Transfer to operator"));
    }

    #[tokio::test]
    async fn direct_provider_rejects_tx_hash_replay() {
        if std::env::var("ANVIL_E2E").unwrap_or_default() != "1" {
            eprintln!("skipping anvil e2e (set ANVIL_E2E=1)");
            return;
        }

        let anvil = AnvilInstance::spawn();
        let rpc = anvil.rpc_url();

        let deployer_signer: PrivateKeySigner = TEST_OPERATOR_KEY.parse().unwrap();
        let deployer_wallet = EthereumWallet::from(deployer_signer.clone());
        let deployer_provider = ProviderBuilder::new()
            .wallet(deployer_wallet)
            .connect_http(rpc.parse().unwrap());

        let token = TestToken::deploy(&deployer_provider).await.unwrap();
        let token_addr = *token.address();

        let caller_signer: PrivateKeySigner = CALLER_KEY.parse().unwrap();
        let caller_addr = caller_signer.address();
        token
            .mint(caller_addr, U256::from(10_000u64))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        let caller_wallet = EthereumWallet::from(caller_signer);
        let caller_provider = ProviderBuilder::new()
            .wallet(caller_wallet)
            .connect_http(rpc.parse().unwrap());
        let caller_token = TestToken::new(token_addr, &caller_provider);

        let tx_receipt = caller_token
            .transfer(deployer_signer.address(), U256::from(5_000u64))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();

        let tx_hash = format!("{:#x}", tx_receipt.transaction_hash);

        let provider = DirectProvider::new(
            rpc,
            TEST_OPERATOR_KEY.into(),
            Some(format!("{:#x}", token_addr)),
            0,
            None, // replay store (in-memory for test)
        )
        .unwrap();

        let proof = PaymentProof::DirectTransfer {
            tx_hash: tx_hash.clone(),
            from: format!("{:#x}", caller_addr),
            amount: "5000".into(),
            token: format!("{:#x}", token_addr),
        };

        // First use: should succeed
        let first = provider.authorize(&proof).await;
        assert!(first.is_ok(), "first use should succeed: {:?}", first.err());

        // Second use: MUST be rejected (replay)
        let second = provider.authorize(&proof).await;
        assert!(second.is_err(), "CRITICAL: tx_hash replay must be rejected");
        assert!(
            second.unwrap_err().to_string().contains("replay"),
            "error should mention replay"
        );
    }

    #[tokio::test]
    async fn direct_provider_rejects_nonexistent_tx() {
        if std::env::var("ANVIL_E2E").unwrap_or_default() != "1" {
            eprintln!("skipping anvil e2e (set ANVIL_E2E=1)");
            return;
        }

        let anvil = AnvilInstance::spawn();
        let provider = DirectProvider::new(
            anvil.rpc_url(),
            TEST_OPERATOR_KEY.into(),
            Some("0x0000000000000000000000000000000000000001".into()),
            0,
            None, // replay store (in-memory for test)
        )
        .unwrap();

        let proof = PaymentProof::DirectTransfer {
            tx_hash: "0x0000000000000000000000000000000000000000000000000000000000000001".into(),
            from: "0x0".into(),
            amount: "100".into(),
            token: "".into(),
        };

        let result = provider.authorize(&proof).await;
        assert!(result.is_err(), "should reject nonexistent tx");
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}

// ─── Direct provider construction tests (no Anvil needed) ─────────────

#[test]
fn direct_provider_rejects_missing_token() {
    use tangle_inference_core::payment::DirectProvider;
    let result = DirectProvider::new(
        "http://localhost:8545".into(),
        TEST_OPERATOR_KEY.into(),
        None, // no token = must fail
        1,
        None, // replay store (in-memory for test)
    );
    assert!(
        result.is_err(),
        "DirectProvider MUST require a payment token"
    );
    let err = format!("{}", result.err().unwrap());
    assert!(
        err.contains("requires payment_token"),
        "error should mention token requirement, got: {err}"
    );
}

#[test]
fn direct_provider_rejects_invalid_rpc_url() {
    use tangle_inference_core::payment::DirectProvider;
    let result = DirectProvider::new(
        "not a url".into(),
        TEST_OPERATOR_KEY.into(),
        Some("0x0000000000000000000000000000000000000001".into()),
        1,
        None, // replay store (in-memory for test)
    );
    assert!(result.is_err());
}

#[test]
fn direct_provider_rejects_invalid_operator_key() {
    use tangle_inference_core::payment::DirectProvider;
    let result = DirectProvider::new(
        "http://localhost:8545".into(),
        "garbage".into(),
        Some("0x0000000000000000000000000000000000000001".into()),
        1,
        None, // replay store (in-memory for test)
    );
    assert!(result.is_err());
}

#[test]
fn direct_provider_rejects_invalid_token_address() {
    use tangle_inference_core::payment::DirectProvider;
    let result = DirectProvider::new(
        "http://localhost:8545".into(),
        TEST_OPERATOR_KEY.into(),
        Some("not-an-address".into()),
        1,
        None, // replay store (in-memory for test)
    );
    assert!(result.is_err());
}
