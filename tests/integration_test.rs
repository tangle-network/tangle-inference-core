//! Real Anvil payment journeys: direct ERC-20 transfers and the ShieldedCredits SpendAuth gate.
use alloy::{
    network::{Ethereum, EthereumWallet, Network, TransactionBuilder},
    primitives::{Address, Bytes, B256, U256},
    providers::{Provider, ProviderBuilder},
    signers::{local::PrivateKeySigner, SignerSync},
    sol,
    sol_types::{eip712_domain, SolStruct},
};
use axum::response::Response;
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
use tangle_inference_core::{
    config::{BillingConfig, ServerConfig, TangleConfig},
    payment::{DirectProvider, PaymentProof, PaymentProvider},
    server::{authorize_request, settle_request, AppState, SpendAuthPayload},
};

const TEST_OPERATOR_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const CALLER_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
// Minimal ERC-20 compiled from tests/TestERC20.sol with solc 0.8.26 --optimize --metadata-hash none
sol! {
    #[sol(rpc, bytecode = "6080604052348015600e575f80fd5b506105268061001c5f395ff3fe608060405234801561000f575f80fd5b5060043610610060575f3560e01c8063095ea7b31461006457806323b872dd1461008c57806340c10f191461009f57806370a08231146100b4578063a9059cbb146100e1578063dd62ed3e146100f4575b5f80fd5b61007761007236600461042c565b61011e565b60405190151581526020015b60405180910390f35b61007761009a366004610454565b61014b565b6100b26100ad36600461042c565b6102bc565b005b6100d36100c236600461048e565b5f6020819052908152604090205481565b604051908152602001610083565b6100776100ef36600461042c565b61032c565b6100d36101023660046104ae565b600160209081525f928352604080842090915290825290205481565b335f9081526001602081815260408084206001600160a01b03871685529091529091208290555b92915050565b6001600160a01b0383165f90815260208190526040812054821180159061019457506001600160a01b0384165f9081526001602090815260408083203384529091529020548211155b6101d45760405162461bcd60e51b815260206004820152600c60248201526b1a5b9cdd59999a58da595b9d60a21b60448201526064015b60405180910390fd5b6001600160a01b0384165f908152600160209081526040808320338452909152812080548492906102069084906104f3565b90915550506001600160a01b0384165f90815260208190526040812080548492906102329084906104f3565b90915550506001600160a01b0383165f908152602081905260408120805484929061025e908490610506565b92505081905550826001600160a01b0316846001600160a01b03167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef846040516102aa91815260200190565b60405180910390a35060019392505050565b6001600160a01b0382165f90815260208190526040812080548392906102e3908490610506565b90915550506040518181526001600160a01b038316905f907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a35050565b335f908152602081905260408120548211156103795760405162461bcd60e51b815260206004820152600c60248201526b1a5b9cdd59999a58da595b9d60a21b60448201526064016101cb565b335f90815260208190526040812080548492906103979084906104f3565b90915550506001600160a01b0383165f90815260208190526040812080548492906103c3908490610506565b90915550506040518281526001600160a01b0384169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a350600192915050565b80356001600160a01b0381168114610427575f80fd5b919050565b5f806040838503121561043d575f80fd5b61044683610411565b946020939093013593505050565b5f805f60608486031215610466575f80fd5b61046f84610411565b925061047d60208501610411565b929592945050506040919091013590565b5f6020828403121561049e575f80fd5b6104a782610411565b9392505050565b5f80604083850312156104bf575f80fd5b6104c883610411565b91506104d660208401610411565b90509250929050565b634e487b7160e01b5f52601160045260245ffd5b81810381811115610145576101456104df565b80820180821115610145576101456104df56fea164736f6c634300081a000a")]
    contract TestToken {
        function mint(address to, uint256 amount) external;
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address owner) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

// tests/fixtures/ShieldedCredits.bin is src/shielded/ShieldedCredits.sol from
// github.com/tangle-network/shielded-payment-gateway rev 65f6b4064503e79fcc750799bc3f69a4e1f6c4ad
// (OpenZeppelin 5.1.0 per its soldeer.lock), compiled with that repo's foundry.toml settings:
// solc 0.8.26, optimizer 200 runs, via_ir, evm cancun, bytecode_hash none.
sol! {
    struct SpendAuthorization {
        bytes32 commitment;
        uint64 serviceId;
        uint8 jobIndex;
        uint256 amount;
        address operator;
        uint256 nonce;
        uint64 expiry;
    }

    #[sol(rpc)]
    contract ShieldedCredits {
        function fundCredits(address token, uint256 amount, bytes32 commitment, address spendingKey) external;
        function getAccount(bytes32 commitment) external view returns (
            address spendingKey,
            address token,
            uint256 balance,
            uint256 totalFunded,
            uint256 totalSpent,
            uint256 nonce
        );
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

#[tokio::test]
async fn direct_transfer_authorization_and_rejections_on_anvil() -> anyhow::Result<()> {
    if std::env::var("ANVIL_E2E").as_deref() != Ok("1") {
        eprintln!("set ANVIL_E2E=1 to run the chain journey");
        return Ok(());
    }
    let anvil = AnvilInstance::spawn();
    let rpc = anvil.rpc_url();
    let operator: PrivateKeySigner = TEST_OPERATOR_KEY.parse()?;
    let recipient = operator.address();
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(operator))
        .connect_http(rpc.parse()?);
    let token = TestToken::deploy(&deployer).await?;
    let token_address = *token.address();
    let caller: PrivateKeySigner = CALLER_KEY.parse()?;
    let sender = caller.address();
    token
        .mint(sender, U256::from(1_000_000u64))
        .send()
        .await?
        .get_receipt()
        .await?;
    assert_eq!(
        token.balanceOf(sender).call().await?,
        U256::from(1_000_000u64)
    );
    let payer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(caller))
        .connect_http(rpc.parse()?);
    let payer_token = TestToken::new(token_address, &payer);
    let provider = DirectProvider::new(
        rpc,
        TEST_OPERATOR_KEY.into(),
        Some(format!("{token_address:#x}")),
        0,
        None,
    )?;
    for (destination, sent, claimed, expected_error) in [
        (recipient, 500_000u64, 500_000u64, None),
        (recipient, 100, 500, Some("less than requested")),
        (recipient, 100, 100, Some("below the required")),
        (
            "0x000000000000000000000000000000000000dEaD".parse::<Address>()?,
            500,
            500,
            Some("no ERC-20 Transfer to operator"),
        ),
    ] {
        let receipt = payer_token
            .transfer(destination, U256::from(sent))
            .send()
            .await?
            .get_receipt()
            .await?;
        let proof = PaymentProof::DirectTransfer {
            tx_hash: format!("{:#x}", receipt.transaction_hash),
            from: format!("{sender:#x}"),
            amount: claimed.to_string(),
            token: format!("{token_address:#x}"),
        };
        match expected_error {
            Some(message) => assert!(provider
                .authorize(&proof, 400)
                .await
                .unwrap_err()
                .to_string()
                .contains(message)),
            None => {
                assert_eq!(provider.authorize(&proof, 400).await?, claimed);
                provider.settle(&proof, 400_000).await?;
                assert!(provider
                    .authorize(&proof, 400)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("replay"));
            }
        }
    }
    let missing = PaymentProof::DirectTransfer {
        tx_hash: format!("0x{:064x}", 1),
        from: format!("{sender:#x}"),
        amount: "100".into(),
        token: format!("{token_address:#x}"),
    };
    assert!(provider
        .authorize(&missing, 0)
        .await
        .unwrap_err()
        .to_string()
        .contains("not found"));
    Ok(())
}

const SERVICE_ID: u64 = 1;
const FUNDED: u64 = 10_000;

/// A funded ShieldedCredits account and the spending key that signs its SpendAuths.
struct Account {
    commitment: B256,
    key: PrivateKeySigner,
}

struct Shielded {
    rpc: String,
    chain_id: u64,
    credits: Address,
    token: Address,
    operator: Address,
}

/// Deploy ShieldedCredits and a test token, and fund `count` credit accounts with FUNDED each.
async fn deploy_shielded(rpc: String, count: u8) -> anyhow::Result<(Shielded, Vec<Account>)> {
    let operator: PrivateKeySigner = TEST_OPERATOR_KEY.parse()?;
    let operator_address = operator.address();
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(operator))
        .connect_http(rpc.parse()?);
    let code: Bytes = include_str!("fixtures/ShieldedCredits.bin")
        .trim()
        .parse()?;
    let deploy = <Ethereum as Network>::TransactionRequest::default().with_deploy_code(code);
    let credits = deployer
        .send_transaction(deploy)
        .await?
        .get_receipt()
        .await?
        .contract_address
        .expect("ShieldedCredits deployment");
    let token = *TestToken::deploy(&deployer).await?.address();
    let funder: PrivateKeySigner = CALLER_KEY.parse()?;
    let funder_address = funder.address();
    let funder = ProviderBuilder::new()
        .wallet(EthereumWallet::from(funder))
        .connect_http(rpc.parse()?);
    let funder_token = TestToken::new(token, &funder);
    let funder_credits = ShieldedCredits::new(credits, &funder);
    let total = U256::from(FUNDED) * U256::from(count);
    funder_token
        .mint(funder_address, total)
        .send()
        .await?
        .get_receipt()
        .await?;
    funder_token
        .approve(credits, total)
        .send()
        .await?
        .get_receipt()
        .await?;
    let mut accounts = Vec::new();
    for tag in 1..=count {
        let account = Account {
            commitment: B256::with_last_byte(tag),
            key: PrivateKeySigner::from_bytes(&B256::with_last_byte(0x10 + tag))?,
        };
        funder_credits
            .fundCredits(
                token,
                U256::from(FUNDED),
                account.commitment,
                account.key.address(),
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        accounts.push(account);
    }
    let chain = Shielded {
        chain_id: deployer.get_chain_id().await?,
        rpc,
        credits,
        token,
        operator: operator_address,
    };
    Ok((chain, accounts))
}

impl Shielded {
    fn sign(
        &self,
        account: &Account,
        signer: &PrivateKeySigner,
        amount: u64,
        nonce: u64,
    ) -> SpendAuthPayload {
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let typed = SpendAuthorization {
            commitment: account.commitment,
            serviceId: SERVICE_ID,
            jobIndex: 0,
            amount: U256::from(amount),
            operator: self.operator,
            nonce: U256::from(nonce),
            expiry,
        };
        let domain = eip712_domain! {
            name: "ShieldedCredits",
            version: "1",
            chain_id: self.chain_id,
            verifying_contract: self.credits,
        };
        let signature = signer
            .sign_hash_sync(&typed.eip712_signing_hash(&domain))
            .unwrap();
        SpendAuthPayload {
            commitment: format!("{:#x}", account.commitment),
            service_id: SERVICE_ID,
            job_index: 0,
            amount: amount.to_string(),
            operator: format!("{:#x}", self.operator),
            nonce,
            expiry,
            signature: alloy::hex::encode_prefixed(signature.as_bytes()),
        }
    }

    /// An operator over this chain; `overrides` replace fields of the default billing config.
    fn state(&self, overrides: serde_json::Value) -> AppState {
        let tangle = TangleConfig {
            rpc_url: self.rpc.clone(),
            chain_id: self.chain_id,
            operator_key: TEST_OPERATOR_KEY.into(),
            shielded_credits: format!("{:#x}", self.credits),
            blueprint_id: 0,
            service_id: Some(SERVICE_ID),
        };
        let mut billing = serde_json::json!({
            "payment_rails": { "shielded": true },
            "max_spend_per_request": 0,
            "min_credit_balance": 0,
            "min_charge_amount": 100,
            "nonce_store_path": null,
            "settlement_journal_path": null,
            "direct_replay_store_path": null,
            "payment_token_address": format!("{:#x}", self.token),
        });
        billing
            .as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        let billing: BillingConfig = serde_json::from_value(billing).unwrap();
        let server: ServerConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        AppState::from_config(&tangle, &server, &billing, 8, ()).unwrap()
    }

    /// On-chain (balance, nonce): authorizeSpend is the only call that moves either.
    async fn on_chain(&self, account: &Account) -> (u64, u64) {
        let provider = ProviderBuilder::new().connect_http(self.rpc.parse().unwrap());
        let view = ShieldedCredits::new(self.credits, provider)
            .getAccount(account.commitment)
            .call()
            .await
            .unwrap();
        (view.balance.to(), view.nonce.to())
    }
}

/// The refusal's status and body; panics if the request was accepted.
async fn refusal<T>(result: Result<T, Response>) -> (u16, String) {
    let response = result.err().expect("request must be refused");
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn spend_auth_gate_floor_ceiling_rail_and_nonce_order_on_anvil() -> anyhow::Result<()> {
    if std::env::var("ANVIL_E2E").as_deref() != Ok("1") {
        eprintln!("set ANVIL_E2E=1 to run the chain journey");
        return Ok(());
    }
    let anvil = AnvilInstance::spawn();
    let (chain, accounts) = deploy_shielded(anvil.rpc_url(), 4).await?;
    let [a, b, c, d] = <[Account; 4]>::try_from(accounts).ok().unwrap();
    let gate = chain.state(serde_json::json!({}));
    let healthy = || async { true };

    // Nonce order: neither an unsigned nor a foreign-signed SpendAuth consumes (commitment, 0).
    let mut unsigned = chain.sign(&a, &a.key, 300, 0);
    unsigned.signature = "0x".into();
    let unsigned = PaymentProof::SpendAuth(unsigned);
    let (status, body) = refusal(authorize_request(&gate, unsigned, 300, healthy()).await).await;
    assert_eq!(status, 402, "{body}");
    assert!(body.contains("\"invalid_spend_auth\""), "{body}");
    let foreign_key = PrivateKeySigner::from_bytes(&B256::with_last_byte(0xff))?;
    let foreign = PaymentProof::SpendAuth(chain.sign(&a, &foreign_key, 300, 0));
    let (status, body) = refusal(authorize_request(&gate, foreign, 300, healthy()).await).await;
    assert_eq!(status, 402, "{body}");
    assert!(body.contains("\"signer_mismatch\""), "{body}");
    // Floor: a SpendAuth equal to max(estimate, min_charge) passes and authorizes on-chain.
    let valid = PaymentProof::SpendAuth(chain.sign(&a, &a.key, 300, 0));
    let at_floor = authorize_request(&gate, valid.clone(), 300, healthy())
        .await
        .map_err(|r| anyhow::anyhow!("valid SpendAuth refused: {}", r.status()))?;
    assert_eq!(at_floor.preauth, Some(300));
    assert_eq!(chain.on_chain(&a).await, (FUNDED - 300, 1));
    // A validly signed replay is refused without touching the chain.
    let replay = authorize_request(&gate, valid, 300, healthy()).await;
    let (status, body) = refusal(replay).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("\"nonce_replay\""), "{body}");
    assert_eq!(chain.on_chain(&a).await, (FUNDED - 300, 1));

    // Ceiling: 1.5 x 300 = 450 is accepted, 451 is refused before authorizeSpend.
    let at_ceiling = PaymentProof::SpendAuth(chain.sign(&a, &a.key, 450, 1));
    let authorized = authorize_request(&gate, at_ceiling, 300, healthy())
        .await
        .map_err(|r| anyhow::anyhow!("SpendAuth at the ceiling refused: {}", r.status()))?;
    assert_eq!(authorized.preauth, Some(450));
    assert_eq!(chain.on_chain(&a).await, (FUNDED - 750, 2));
    let over = PaymentProof::SpendAuth(chain.sign(&a, &a.key, 451, 2));
    let (status, body) = refusal(authorize_request(&gate, over, 300, healthy()).await).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("\"excessive_preauth\""), "{body}");
    assert_eq!(chain.on_chain(&a).await, (FUNDED - 750, 2));
    // A refusal after validation frees the nonce: the contract requires nonce 2 next.
    let retry = PaymentProof::SpendAuth(chain.sign(&a, &a.key, 300, 2));
    let retried = authorize_request(&gate, retry, 300, healthy())
        .await
        .map_err(|r| anyhow::anyhow!("retry after excessive_preauth refused: {}", r.status()))?;
    assert_eq!(chain.on_chain(&a).await, (FUNDED - 1050, 3));
    // The backend fails after authorizeSpend: dropping the unsettled payment releases its 300.
    drop(retried);
    gate.drain_settlements().await;
    assert_eq!(chain.on_chain(&a).await, (FUNDED - 750, 3));

    // Floor: one unit under the estimate is a 402 quoting the floor, before authorizeSpend.
    let short = PaymentProof::SpendAuth(chain.sign(&b, &b.key, 299, 0));
    let response = authorize_request(&gate, short, 300, healthy())
        .await
        .err()
        .unwrap();
    assert_eq!(response.headers()["X-Payment-Required"], "300");
    let (status, body) = refusal::<()>(Err(response)).await;
    assert_eq!(status, 402, "{body}");
    assert!(body.contains("\"payment_required\""), "{body}");
    assert_eq!(chain.on_chain(&b).await, (FUNDED, 0));
    let retry = PaymentProof::SpendAuth(chain.sign(&b, &b.key, 300, 0));
    let retried = authorize_request(&gate, retry, 300, healthy())
        .await
        .map_err(|r| anyhow::anyhow!("retry after the floor refusal refused: {}", r.status()))?;
    assert_eq!(chain.on_chain(&b).await, (FUNDED - 300, 1));
    // A caller that fails to serve drops the payment, which releases the credit.
    drop(retried);
    gate.drain_settlements().await;
    assert_eq!(chain.on_chain(&b).await, (FUNDED, 1));

    // Ceiling never below the floor: min_charge 1000 > 1.5 x 100, so exactly 1000 is accepted.
    let floor_gate = chain.state(serde_json::json!({ "min_charge_amount": 1000 }));
    let at_floor = PaymentProof::SpendAuth(chain.sign(&c, &c.key, 1000, 0));
    let authorized = authorize_request(&floor_gate, at_floor, 100, healthy())
        .await
        .map_err(|r| anyhow::anyhow!("SpendAuth at the floor refused: {}", r.status()))?;
    assert_eq!(authorized.preauth, Some(1000));
    assert_eq!(chain.on_chain(&c).await, (FUNDED - 1000, 1));
    // A settlement spawned past the response still lands when the server drains at shutdown.
    let settling = floor_gate.clone();
    floor_gate
        .spawn_settlement(async move { settle_request(&settling, &authorized, Some(100)).await });
    floor_gate.drain_settlements().await;
    // Settling 100 of the 1000 pre-authorized refunds the other 900.
    assert_eq!(chain.on_chain(&c).await, (FUNDED - 100, 1));
    let over = PaymentProof::SpendAuth(chain.sign(&c, &c.key, 1001, 1));
    let (status, body) = refusal(authorize_request(&floor_gate, over, 100, healthy()).await).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("\"excessive_preauth\""), "{body}");
    assert_eq!(chain.on_chain(&c).await, (FUNDED - 100, 1));

    // Rail disabled: a direct-only operator refuses a valid SpendAuth.
    let direct_only = chain.state(serde_json::json!({ "payment_rails": { "direct": true } }));
    let valid = PaymentProof::SpendAuth(chain.sign(&d, &d.key, 300, 0));
    let (status, body) =
        refusal(authorize_request(&direct_only, valid, 300, healthy()).await).await;
    assert_eq!(status, 402, "{body}");
    assert!(body.contains("\"rail_disabled\""), "{body}");
    assert_eq!(chain.on_chain(&d).await, (FUNDED, 0));

    // A never-mined authorizeSpend is a 402 after the receipt timeout, not a hung request.
    let impatient = chain.state(serde_json::json!({ "receipt_timeout_secs": 2 }));
    let rpc = ProviderBuilder::new().connect_http(chain.rpc.parse()?);
    rpc.raw_request::<_, ()>("anvil_setAutomine".into(), (false,))
        .await?;
    let started = std::time::Instant::now();
    let valid = PaymentProof::SpendAuth(chain.sign(&d, &d.key, 300, 0));
    let (status, body) = refusal(authorize_request(&impatient, valid, 300, healthy()).await).await;
    let waited = started.elapsed();
    rpc.raw_request::<_, ()>("anvil_setAutomine".into(), (true,))
        .await?;
    assert_eq!(status, 402, "{body}");
    assert!(body.contains("\"authorization_failed\""), "{body}");
    assert!(waited < Duration::from_secs(5), "refused after {waited:?}");
    // Mined late, the authorization locks its 300 with no operator holding it: expiry frees it.
    rpc.raw_request::<_, serde_json::Value>("evm_mine".into(), ())
        .await?;
    assert_eq!(chain.on_chain(&d).await, (FUNDED - 300, 1));
    Ok(())
}

/// Owed minus settled records in the journal file: settlements a restart must still send.
fn owed_in(journal: &std::path::Path) -> usize {
    let text = std::fs::read_to_string(journal).unwrap_or_default();
    text.matches("{\"owed\"").count() - text.matches("{\"settled\"").count()
}

/// Set the next block's base fee and mine it, so the operator's gas-price cap holds or passes.
async fn set_base_fee(rpc: &str, gwei: u64) -> anyhow::Result<()> {
    let provider = ProviderBuilder::new().connect_http(rpc.parse()?);
    let wei = U256::from(gwei) * U256::from(1_000_000_000u64);
    provider
        .raw_request::<_, serde_json::Value>("anvil_setNextBlockBaseFeePerGas".into(), (wei,))
        .await?;
    provider
        .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
        .await?;
    Ok(())
}

#[test]
fn killed_operator_settles_each_served_request_once_after_restart_on_anvil() -> anyhow::Result<()> {
    if std::env::var("ANVIL_E2E").as_deref() != Ok("1") {
        eprintln!("set ANVIL_E2E=1 to run the chain journey");
        return Ok(());
    }
    let anvil = AnvilInstance::spawn();
    let rpc = anvil.rpc_url();
    let dir = tempfile::tempdir()?;
    let journal = dir.path().join("data/settlements.jsonl");
    let config = serde_json::json!({
        "settlement_journal_path": journal,
        "max_gas_price_gwei": 100,
    });
    let main = tokio::runtime::Runtime::new()?;
    let (chain, accounts) = main.block_on(deploy_shielded(rpc.clone(), 1))?;
    let account = &accounts[0];
    let costs = [100u64, 150, 200, 250];

    // The operator serves four requests and settles the first; the other three settlements are
    // journaled and held by a gas spike when the operator is killed: no drain, no shutdown.
    let operator = tokio::runtime::Runtime::new()?;
    operator.block_on(async {
        let state = chain.state(config.clone());
        let mut served = Vec::new();
        for (nonce, cost) in (0u64..).zip(costs) {
            let proof = PaymentProof::SpendAuth(chain.sign(account, &account.key, 300, nonce));
            let authorized = authorize_request(&state, proof, 300, async { true })
                .await
                .map_err(|r| anyhow::anyhow!("request {nonce} refused: {}", r.status()))?;
            served.push((authorized, cost));
        }
        let (first, cost) = served.remove(0);
        settle_request(&state, &first, Some(cost)).await;
        assert_eq!(owed_in(&journal), 0);
        set_base_fee(&rpc, 1_000).await?;
        for (authorized, cost) in served {
            let settling = state.clone();
            state.spawn_settlement(async move {
                settle_request(&settling, &authorized, Some(cost)).await
            });
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while owed_in(&journal) < 3 {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "settlements never journaled"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })?;
    // Cancel every task without draining it; the wait only lets the dropped journal unlock.
    operator.shutdown_timeout(Duration::from_secs(10));
    let served: u64 = costs.iter().sum();
    let (url, token, operator) = (rpc.as_str(), chain.token, chain.operator);
    let operator_paid = move || async move {
        let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
        let paid = TestToken::new(token, provider)
            .balanceOf(operator)
            .call()
            .await
            .unwrap();
        paid.to::<u64>()
    };
    main.block_on(async {
        // Only the first settlement landed; the other three still lock their 300 pre-auths.
        assert_eq!(owed_in(&journal), 3);
        assert_eq!(operator_paid().await, costs[0]);
        assert_eq!(chain.on_chain(account).await, (FUNDED - costs[0] - 900, 4));
        set_base_fee(&rpc, 1).await?;

        // A fresh operator on the same journal settles every owed request.
        let owed = std::fs::read(&journal)?;
        let restarted = chain.state(config.clone());
        restarted.drain_settlements().await;
        assert_eq!(owed_in(&journal), 0);
        assert_eq!(operator_paid().await, served);
        assert_eq!(chain.on_chain(account).await, (FUNDED - served, 4));
        drop(restarted);

        // Killed again after confirming but before forgetting them: replay charges nothing twice.
        std::fs::write(&journal, owed)?;
        let replayed = chain.state(config.clone());
        replayed.drain_settlements().await;
        assert_eq!(owed_in(&journal), 0);
        assert_eq!(operator_paid().await, served);
        assert_eq!(chain.on_chain(account).await, (FUNDED - served, 4));
        anyhow::Ok(())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_authorize_and_settle_each_once_on_anvil() -> anyhow::Result<()> {
    if std::env::var("ANVIL_E2E").as_deref() != Ok("1") {
        eprintln!("set ANVIL_E2E=1 to run the chain journey");
        return Ok(());
    }
    let anvil = AnvilInstance::spawn();
    let (chain, accounts) = deploy_shielded(anvil.rpc_url(), 8).await?;
    // No retries: each concurrent operator transaction must land on its first send.
    let state = chain.state(serde_json::json!({ "claim_max_retries": 0 }));
    let costs: Vec<u64> = (0..8).map(|i| 100 + 10 * i).collect();
    let mut requests = tokio::task::JoinSet::new();
    for (account, &cost) in accounts.iter().zip(&costs) {
        let proof = PaymentProof::SpendAuth(chain.sign(account, &account.key, 300, 0));
        let state = state.clone();
        requests.spawn(async move {
            match authorize_request(&state, proof, 300, async { true }).await {
                Ok(authorized) => Ok((authorized, cost)),
                Err(response) => Err(refusal::<()>(Err(response)).await),
            }
        });
    }
    let (authorized, refused): (Vec<_>, Vec<_>) = requests
        .join_all()
        .await
        .into_iter()
        .partition(Result::is_ok);
    let refused: Vec<_> = refused.into_iter().filter_map(Result::err).collect();
    assert!(
        refused.is_empty(),
        "{} of 8 refused: {refused:?}",
        refused.len()
    );
    for account in &accounts {
        assert_eq!(chain.on_chain(account).await, (FUNDED - 300, 1));
    }

    for (authorized, cost) in authorized.into_iter().filter_map(Result::ok) {
        let settling = state.clone();
        state.spawn_settlement(
            async move { settle_request(&settling, &authorized, Some(cost)).await },
        );
    }
    state.drain_settlements().await;
    let provider = ProviderBuilder::new().connect_http(chain.rpc.parse()?);
    let paid = TestToken::new(chain.token, provider)
        .balanceOf(chain.operator)
        .call()
        .await?;
    assert_eq!(paid, U256::from(costs.iter().sum::<u64>()));
    for (account, cost) in accounts.iter().zip(costs) {
        assert_eq!(chain.on_chain(account).await, (FUNDED - cost, 1));
    }
    Ok(())
}
