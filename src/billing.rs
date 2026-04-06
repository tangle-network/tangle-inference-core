use std::time::Duration;

use alloy::{
    network::EthereumWallet,
    primitives::{keccak256, Address, FixedBytes, B256, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
    sol_types::SolValue,
};

use crate::config::{default_claim_max_retries, BillingConfig, TangleConfig};
use crate::server::SpendAuthPayload;

/// Trait for computing request cost. Blueprints implement this to define
/// their own pricing model (per-token, per-second, flat rate, etc.).
pub trait CostModel: Send + Sync + 'static {
    /// Calculate cost in base token units for the given request parameters.
    /// Returns the cost that should be charged.
    fn calculate_cost(&self, params: &CostParams) -> u64;
}

/// Parameters passed to `CostModel::calculate_cost`.
#[derive(Debug, Clone, Default)]
pub struct CostParams {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Optional task type for task-aware pricing models
    /// (e.g. "chat", "tts", "stt", "image", "video", "embedding").
    pub task_type: Option<String>,
    /// Arbitrary key-value pairs for backend-specific cost inputs
    /// (e.g. GPU-seconds, image dimensions, characters, centiseconds).
    pub extra: std::collections::HashMap<String, u64>,
}

/// Per-token pricing — classic LLM inference model.
pub struct PerTokenCostModel {
    pub price_per_input_token: u64,
    pub price_per_output_token: u64,
}

impl CostModel for PerTokenCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        (params.prompt_tokens as u64) * self.price_per_input_token
            + (params.completion_tokens as u64) * self.price_per_output_token
    }
}

/// Per-1K-character pricing — TTS blueprints.
/// Reads `extra["characters"]`.
pub struct PerCharCostModel {
    pub price_per_1k_chars: u64,
}

impl CostModel for PerCharCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        let chars = params.extra.get("characters").copied().unwrap_or(0);
        (chars * self.price_per_1k_chars) / 1000
    }
}

/// Per-second pricing — audio transcription, video generation.
/// Reads `extra["centiseconds"]` (1 second = 100 centiseconds) for sub-second
/// granularity without floats.
pub struct PerSecondCostModel {
    pub price_per_second: u64,
}

impl CostModel for PerSecondCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        let centiseconds = params.extra.get("centiseconds").copied().unwrap_or(0);
        (centiseconds * self.price_per_second) / 100
    }
}

/// Per-image pricing — image generation.
/// Reads `extra["images"]`, defaults to 1.
pub struct PerImageCostModel {
    pub price_per_image: u64,
}

impl CostModel for PerImageCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        let images = params.extra.get("images").copied().unwrap_or(1);
        images * self.price_per_image
    }
}

/// Flat per-request pricing.
pub struct FlatRequestCostModel {
    pub price_per_request: u64,
}

impl CostModel for FlatRequestCostModel {
    fn calculate_cost(&self, _params: &CostParams) -> u64 {
        self.price_per_request
    }
}

/// Task-type-aware pricing — dispatches to a sub-model based on `task_type`.
/// Used by Modal which serves multiple task types under one operator.
pub struct TaskTypeCostModel {
    pub default: Box<dyn CostModel>,
    pub per_task: std::collections::HashMap<String, Box<dyn CostModel>>,
}

impl CostModel for TaskTypeCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        if let Some(task) = &params.task_type {
            if let Some(model) = self.per_task.get(task) {
                return model.calculate_cost(params);
            }
        }
        self.default.calculate_cost(params)
    }
}

// Generate bindings for the ShieldedCredits contract.
sol! {
    #[sol(rpc)]
    interface IShieldedCredits {
        struct SpendAuth {
            bytes32 commitment;
            uint64 serviceId;
            uint8 jobIndex;
            uint256 amount;
            address operator;
            uint256 nonce;
            uint64 expiry;
            bytes signature;
        }

        function authorizeSpend(SpendAuth calldata auth) external returns (bytes32 authHash);
        function claimPayment(bytes32 authHash, address recipient) external;
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

/// On-chain account info returned by getAccount.
pub struct AccountInfo {
    pub spending_key: Address,
    pub balance: U256,
}

/// Handles ShieldedCredits billing operations.
pub struct BillingClient {
    claim_max_retries: u32,
    max_gas_price_gwei: u64,
    service_id: u64,
    wallet: EthereumWallet,
    shielded_credits: Address,
    rpc_url: reqwest::Url,
    operator_address: Address,
}

impl BillingClient {
    /// Construct a `BillingClient` from individual parameters. This is the
    /// preferred constructor for blueprints that don't use the
    /// [`crate::config::TangleConfig`] / [`crate::config::BillingConfig`]
    /// structs.
    pub fn new_with_params(
        rpc_url: String,
        operator_key_hex: String,
        shielded_credits_address: Address,
        service_id: u64,
        max_gas_price_gwei: u64,
    ) -> anyhow::Result<Self> {
        Self::new_with_params_full(
            rpc_url,
            operator_key_hex,
            shielded_credits_address,
            service_id,
            max_gas_price_gwei,
            default_claim_max_retries(),
        )
    }

    /// Same as [`Self::new_with_params`] but lets the caller override
    /// `claim_max_retries`.
    pub fn new_with_params_full(
        rpc_url: String,
        operator_key_hex: String,
        shielded_credits_address: Address,
        service_id: u64,
        max_gas_price_gwei: u64,
        claim_max_retries: u32,
    ) -> anyhow::Result<Self> {
        let signer: PrivateKeySigner = operator_key_hex.parse()?;
        let operator_address = signer.address();

        if std::env::var("PRODUCTION").unwrap_or_default() == "1" {
            anyhow::bail!(
                "PRODUCTION=1 but operator_key is loaded from plaintext. \
                 Use a KMS (AWS KMS, HashiCorp Vault) or encrypted keystore. \
                 Unset PRODUCTION to run in dev mode."
            );
        }
        tracing::warn!(
            "operator_key loaded from plaintext — \
             use a KMS or encrypted keystore in production (set PRODUCTION=1 to enforce)"
        );

        let wallet = EthereumWallet::from(signer);
        let rpc_url: reqwest::Url = rpc_url.parse()?;

        Ok(Self {
            claim_max_retries,
            max_gas_price_gwei,
            service_id,
            wallet,
            shielded_credits: shielded_credits_address,
            rpc_url,
            operator_address,
        })
    }

    /// Convenience constructor that pulls fields from full config structs.
    pub fn new(tangle: &TangleConfig, billing: &BillingConfig) -> anyhow::Result<Self> {
        let shielded_credits: Address = tangle.shielded_credits.parse()?;
        Self::new_with_params_full(
            tangle.rpc_url.clone(),
            tangle.operator_key.clone(),
            shielded_credits,
            tangle.service_id.unwrap_or(0),
            billing.max_gas_price_gwei,
            billing.claim_max_retries,
        )
    }

    /// Returns the configured service_id this client claims payments for.
    pub fn service_id(&self) -> u64 {
        self.service_id
    }

    /// Returns the operator's Ethereum address.
    pub fn operator_address(&self) -> Address {
        self.operator_address
    }

    fn build_auth(
        &self,
        spend_auth: &SpendAuthPayload,
    ) -> anyhow::Result<IShieldedCredits::SpendAuth> {
        let commitment: B256 = spend_auth.commitment.parse()?;
        let amount: U256 = spend_auth.amount.parse()?;
        let operator: Address = spend_auth.operator.parse()?;
        let sig_bytes = hex::decode(
            spend_auth
                .signature
                .strip_prefix("0x")
                .unwrap_or(&spend_auth.signature),
        )?;

        Ok(IShieldedCredits::SpendAuth {
            commitment: FixedBytes(commitment.0),
            serviceId: spend_auth.service_id,
            jobIndex: spend_auth.job_index,
            amount,
            operator,
            nonce: U256::from(spend_auth.nonce),
            expiry: spend_auth.expiry,
            signature: sig_bytes.into(),
        })
    }

    fn auth_hash(spend_auth: &SpendAuthPayload) -> anyhow::Result<FixedBytes<32>> {
        let commitment: B256 = spend_auth.commitment.parse()?;
        let hash = keccak256(
            (
                FixedBytes::<32>(commitment.0),
                U256::from(spend_auth.service_id),
                U256::from(spend_auth.job_index),
                U256::from(spend_auth.nonce),
            )
                .abi_encode(),
        );
        Ok(FixedBytes(hash.0))
    }

    /// Check current gas price against the configured cap.
    async fn check_gas_price(&self) -> anyhow::Result<()> {
        let max_gwei = self.max_gas_price_gwei;
        if max_gwei == 0 {
            return Ok(());
        }

        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());
        let gas_price = provider.get_gas_price().await?;
        let gas_price_gwei = gas_price / 1_000_000_000;

        if gas_price_gwei > max_gwei as u128 {
            anyhow::bail!(
                "gas price {gas_price_gwei} gwei exceeds cap {max_gwei} gwei — deferring tx"
            );
        }

        Ok(())
    }

    /// Pre-authorize spending on-chain. Must be called before serving inference.
    pub async fn authorize_spend(&self, spend_auth: &SpendAuthPayload) -> anyhow::Result<()> {
        self.check_gas_price().await?;

        let auth = self.build_auth(spend_auth)?;

        let provider = ProviderBuilder::new()
            .wallet(self.wallet.clone())
            .connect_http(self.rpc_url.clone());

        let contract = IShieldedCredits::new(self.shielded_credits, &provider);

        let pending = contract.authorizeSpend(auth).send().await?;
        let receipt = pending.get_receipt().await?;
        tracing::info!(
            tx_hash = %receipt.transaction_hash,
            "authorizeSpend confirmed"
        );

        Ok(())
    }

    /// Claim payment on-chain after inference is served.
    ///
    /// The ShieldedCredits contract `claimPayment(bytes32, address)` settles the
    /// full pre-authorized amount. `actual_amount` is logged for auditing only.
    ///
    /// Retries up to `claim_max_retries` times with exponential backoff.
    pub async fn claim_payment(
        &self,
        spend_auth: &SpendAuthPayload,
        actual_amount: u64,
    ) -> anyhow::Result<()> {
        let auth_hash = Self::auth_hash(spend_auth)?;
        let operator: Address = spend_auth.operator.parse()?;
        let max_retries = self.claim_max_retries;

        tracing::info!(
            actual_amount = actual_amount,
            preauth_amount = %spend_auth.amount,
            "claiming payment (actual metered cost)"
        );

        let mut last_err = None;
        for attempt in 0..=max_retries {
            // Backoff before retries (not before the first attempt).
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retrying claimPayment"
                );
                tokio::time::sleep(delay).await;
            }

            // Check gas price — if too high, skip this attempt (loop will
            // backoff on the next iteration via the block above).
            if let Err(e) = self.check_gas_price().await {
                tracing::warn!(error = %e, attempt, "gas price check failed for claimPayment");
                last_err = Some(e);
                continue;
            }

            let provider = ProviderBuilder::new()
                .wallet(self.wallet.clone())
                .connect_http(self.rpc_url.clone());

            let contract = IShieldedCredits::new(self.shielded_credits, &provider);

            match contract.claimPayment(auth_hash, operator).send().await {
                Ok(pending) => match pending.get_receipt().await {
                    Ok(receipt) => {
                        tracing::info!(
                            tx_hash = %receipt.transaction_hash,
                            actual_amount = actual_amount,
                            attempt,
                            "claimPayment confirmed"
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        last_err = Some(e.into());
                    }
                },
                Err(e) => {
                    last_err = Some(e.into());
                }
            }
        }

        let err = last_err.unwrap_or_else(|| anyhow::anyhow!("claimPayment failed"));
        tracing::error!(
            error = %err,
            auth_hash = %auth_hash,
            actual_amount,
            commitment = %spend_auth.commitment,
            "claimPayment FAILED after {} retries — operator served inference for free. Manual recovery required.",
            max_retries
        );
        Err(err)
    }

    /// Query on-chain account info (spending key + balance).
    pub async fn get_account_info(&self, commitment: &str) -> anyhow::Result<AccountInfo> {
        let commitment: B256 = commitment.parse()?;

        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());

        let contract = IShieldedCredits::new(self.shielded_credits, &provider);

        let result = contract.getAccount(FixedBytes(commitment.0)).call().await?;
        Ok(AccountInfo {
            spending_key: result.spendingKey,
            balance: result.balance,
        })
    }
}

/// Verify a SpendAuth EIP-712 signature matches the expected spending key.
///
/// Returns Ok(recovered_address) if valid and matching, Err with reason otherwise.
pub fn verify_spend_auth_signature(
    auth: &SpendAuthPayload,
    expected_spending_key: Address,
    shielded_credits_addr: &str,
    chain_id: u64,
    clock_skew_tolerance_secs: u64,
) -> Result<Address, String> {
    let recovered = recover_spend_auth_signer(
        auth,
        shielded_credits_addr,
        chain_id,
        clock_skew_tolerance_secs,
    )?;

    if recovered != expected_spending_key {
        return Err(format!(
            "recovered signer ({recovered}) does not match expected spending key ({expected_spending_key})"
        ));
    }

    Ok(recovered)
}

/// Recover the signer address from a SpendAuth EIP-712 signature.
///
/// The caller MUST compare the returned address against the account's on-chain
/// spending key to authenticate the request. Also checks expiry with clock skew
/// tolerance.
pub fn recover_spend_auth_signer(
    auth: &SpendAuthPayload,
    shielded_credits_addr: &str,
    chain_id: u64,
    clock_skew_tolerance_secs: u64,
) -> Result<Address, String> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    let shielded_addr: Address = shielded_credits_addr
        .parse()
        .map_err(|e| format!("invalid shielded_credits address: {e}"))?;

    let domain_separator = keccak256(
        (
            keccak256(
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            ),
            keccak256(b"ShieldedCredits"),
            keccak256(b"1"),
            U256::from(chain_id),
            shielded_addr,
        )
            .abi_encode(),
    );

    let spend_typehash = keccak256(
        b"SpendAuthorization(bytes32 commitment,uint64 serviceId,uint8 jobIndex,uint256 amount,address operator,uint256 nonce,uint64 expiry)",
    );

    let commitment: B256 = auth
        .commitment
        .parse()
        .map_err(|e| format!("invalid commitment: {e}"))?;
    let amount: U256 = auth
        .amount
        .parse()
        .map_err(|e| format!("invalid amount: {e}"))?;
    let operator: Address = auth
        .operator
        .parse()
        .map_err(|e| format!("invalid operator address: {e}"))?;

    let struct_hash = keccak256(
        (
            spend_typehash,
            commitment,
            U256::from(auth.service_id),
            U256::from(auth.job_index),
            amount,
            operator,
            U256::from(auth.nonce),
            U256::from(auth.expiry),
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

    let sig_hex = auth.signature.strip_prefix("0x").unwrap_or(&auth.signature);
    let sig_bytes = hex::decode(sig_hex).map_err(|e| format!("invalid signature hex: {e}"))?;
    if sig_bytes.len() != 65 {
        return Err(format!(
            "invalid signature length: expected 65, got {}",
            sig_bytes.len()
        ));
    }

    let v = sig_bytes[64];
    let recovery_id = match v {
        27 => 0u8,
        28 => 1u8,
        0 | 1 => v,
        _ => return Err(format!("invalid signature recovery byte: {v}")),
    };

    let signature =
        Signature::from_slice(&sig_bytes[..64]).map_err(|e| format!("invalid signature: {e}"))?;
    let rid = RecoveryId::try_from(recovery_id).map_err(|e| format!("invalid recovery id: {e}"))?;
    let recovered = VerifyingKey::recover_from_prehash(digest.as_slice(), &signature, rid)
        .map_err(|e| format!("ecrecover failed: {e}"))?;

    let pubkey_bytes = recovered.to_encoded_point(false);
    let pubkey_hash = keccak256(&pubkey_bytes.as_bytes()[1..]);
    let recovered_address = Address::from_slice(&pubkey_hash[12..]);

    // Check expiry with clock skew tolerance
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock is before UNIX epoch".to_string())?
        .as_secs();
    if now > auth.expiry.saturating_add(clock_skew_tolerance_secs) {
        return Err(format!(
            "SpendAuth expired: now={now}, expiry={}, tolerance={clock_skew_tolerance_secs}s",
            auth.expiry
        ));
    }

    Ok(recovered_address)
}
