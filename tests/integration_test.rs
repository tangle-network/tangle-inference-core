//! Replay, pre-serve payment gates and real Anvil ERC-20 integration.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;

use axum::http::StatusCode;

use tangle_inference_core::server::validate_spend_auth;
use tangle_inference_core::{
    AppState, AppStateBuilder, BillingClient, BillingConfig, NonceStore, ServerConfig,
    SpendAuthPayload, TangleConfig,
};

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
        payment_rails: tangle_inference_core::PaymentRails::SHIELDED,
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

    let reloaded = NonceStore::load(Some(path));
    assert!(reloaded.check_replay(&key, 30).await);
}

// --- Payment-gate fixtures ---

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
        .backend(())
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

use tangle_inference_core::payment::{PaymentProof, PaymentProvider, PaymentRails, PaymentRouter};

/// Anvil default account #0 private key.
const TEST_OPERATOR_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[tokio::test]
async fn router_rejects_proof_for_disabled_rail() {
    // A shielded-only router must reject a DirectTransfer proof up front —
    // without touching the chain — rather than silently serving.
    let tangle = test_tangle_config();
    let billing = test_billing_config();
    let router = PaymentRouter::build(PaymentRails::SHIELDED, &tangle, &billing).unwrap();
    let direct = PaymentProof::DirectTransfer {
        tx_hash: "0x00".into(),
        from: "0x00".into(),
        amount: "1".into(),
        token: "0x01".into(),
    };
    let err = router.authorize(&direct).await.unwrap_err();
    assert!(
        err.to_string().contains("direct rail not enabled"),
        "expected disabled-rail rejection, got: {err}"
    );
}

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

        let deployer_signer: PrivateKeySigner = TEST_OPERATOR_KEY.parse().unwrap();
        let deployer_wallet = EthereumWallet::from(deployer_signer.clone());
        let deployer_provider = ProviderBuilder::new()
            .wallet(deployer_wallet)
            .connect_http(rpc.parse().unwrap());

        let token = TestToken::deploy(&deployer_provider).await.unwrap();
        let token_addr = *token.address();

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

        let bal = token.balanceOf(caller_addr).call().await.unwrap();
        assert_eq!(bal, mint_amount);

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

        let provider = DirectProvider::new(
            rpc.clone(),
            TEST_OPERATOR_KEY.into(),
            Some(format!("{:#x}", token_addr)),
            0,    // 0 confirmations for test (anvil auto-mines),
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("less than requested")
        );
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no ERC-20 Transfer to operator")
        );
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
