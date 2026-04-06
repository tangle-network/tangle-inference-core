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

use tangle_inference_core::billing::{recover_spend_auth_signer, verify_spend_auth_signature};
use tangle_inference_core::{
    AppStateBuilder, BillingClient, BillingConfig, CostModel, CostParams, FlatRequestCostModel,
    NonceStore, PerCharCostModel, PerImageCostModel, PerSecondCostModel, PerTokenCostModel,
    ServerConfig, SpendAuthPayload, TangleConfig, TaskTypeCostModel,
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
        billing_required: true,
        max_spend_per_request: 1_000_000,
        min_credit_balance: 1_000,
        min_charge_amount: 100,
        claim_max_retries: 3,
        clock_skew_tolerance_secs: 30,
        max_gas_price_gwei: 0,
        nonce_store_path: None,
        payment_token_address: None,
    }
}

#[tokio::test]
async fn app_state_builder_constructs_and_clones_across_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let nonce_path = tmp.path().join("nonces.json");

    let billing = BillingClient::new(&test_tangle_config(), &test_billing_config())
        .expect("billing client");
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
    let m = PerSecondCostModel { price_per_second: 1_000 };
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
    let m = PerImageCostModel { price_per_image: 7_500 };
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
    let m = FlatRequestCostModel { price_per_request: 999 };
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
        Box::new(PerCharCostModel { price_per_1k_chars: 4_000 }),
    );
    per_task.insert(
        "image".into(),
        Box::new(PerImageCostModel { price_per_image: 50_000 }),
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

    verify_spend_auth_signature(&payload, signing_address, shielded_credits_addr, chain_id, 30)
        .expect("matching signer");

    // Mismatched expected key -> Err.
    let other: Address = "0x000000000000000000000000000000000000bEEF".parse().unwrap();
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
