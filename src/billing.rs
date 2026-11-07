use std::time::Duration;

use alloy::{
    network::EthereumWallet,
    primitives::{keccak256, Address, FixedBytes, B256, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
    sol_types::SolValue,
};

use crate::config::{BillingConfig, TangleConfig};
use crate::journal::{Settlement, SettlementJournal};
use crate::server::SpendAuthPayload;

/// Blueprint pricing in base token units.
pub trait CostModel: Send + Sync + 'static {
    fn calculate_cost(&self, params: &CostParams) -> u64;
}

#[derive(Debug, Clone, Default)]
pub struct CostParams {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Backend task type for pricing by task.
    pub task_type: Option<String>,
    /// Backend quantities keyed by the cost models below.
    pub extra: std::collections::HashMap<String, u64>,
}

pub struct PerTokenCostModel {
    pub price_per_input_token: u64,
    pub price_per_output_token: u64,
}

impl CostModel for PerTokenCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        (params.prompt_tokens as u64)
            .saturating_mul(self.price_per_input_token)
            .saturating_add(
                (params.completion_tokens as u64).saturating_mul(self.price_per_output_token),
            )
    }
}

/// Per-thousand-character pricing from `extra["characters"]`.
pub struct PerCharCostModel {
    pub price_per_1k_chars: u64,
}

impl CostModel for PerCharCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        let chars = params.extra.get("characters").copied().unwrap_or(0);
        chars.saturating_mul(self.price_per_1k_chars) / 1000
    }
}

/// Per-second pricing from `extra["centiseconds"]`, in hundredths of a second.
pub struct PerSecondCostModel {
    pub price_per_second: u64,
}

impl CostModel for PerSecondCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        let centiseconds = params.extra.get("centiseconds").copied().unwrap_or(0);
        centiseconds.saturating_mul(self.price_per_second) / 100
    }
}

/// Per-image pricing from `extra["images"]`, defaulting to one image.
pub struct PerImageCostModel {
    pub price_per_image: u64,
}

impl CostModel for PerImageCostModel {
    fn calculate_cost(&self, params: &CostParams) -> u64 {
        let images = params.extra.get("images").copied().unwrap_or(1);
        images.saturating_mul(self.price_per_image)
    }
}

pub struct FlatRequestCostModel {
    pub price_per_request: u64,
}

impl CostModel for FlatRequestCostModel {
    fn calculate_cost(&self, _params: &CostParams) -> u64 {
        self.price_per_request
    }
}

/// Dispatches known task types to their model, otherwise using the default.
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
        function settlePayment(bytes32 authHash, address recipient, uint256 amount) external;
        function releasePayment(bytes32 authHash) external;
        function getSpendAuth(bytes32 authHash) external view returns (
            uint256 amount,
            address token,
            address operator,
            uint64 expiry,
            bool claimed
        );
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

pub struct BillingClient {
    claim_max_retries: u32,
    max_gas_price_gwei: u64,
    service_id: u64,
    wallet: EthereumWallet,
    shielded_credits: Address,
    rpc_url: reqwest::Url,
    operator_address: Address,
    journal: Option<std::sync::Arc<SettlementJournal>>,
}

impl BillingClient {
    pub fn new(tangle: &TangleConfig, billing: &BillingConfig) -> anyhow::Result<Self> {
        let signer: PrivateKeySigner = tangle.operator_key.parse()?;
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
        let rpc_url: reqwest::Url = tangle.rpc_url.parse()?;

        Ok(Self {
            claim_max_retries: billing.claim_max_retries,
            max_gas_price_gwei: billing.max_gas_price_gwei,
            service_id: tangle.service_id.unwrap_or(0),
            wallet,
            shielded_credits: tangle.shielded_credits.parse()?,
            rpc_url,
            operator_address,
            journal: match billing.settlement_journal_path.clone() {
                Some(path) => SettlementJournal::open(path)?.map(std::sync::Arc::new),
                None => {
                    tracing::warn!(
                        "settlement_journal_path not configured — an operator killed before \
                         settlement confirms is never paid for the work it served"
                    );
                    None
                }
            },
        })
    }

    /// Settle every journaled settlement left by an earlier process, once each.
    pub async fn settle_owed(&self) {
        let Some(journal) = &self.journal else { return };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        for settlement in journal.owed() {
            let hash = &settlement.auth_hash;
            // Skip what the chain already claimed or expired, so replay never charges twice.
            let open = settlement.spend_auth.expiry >= now
                && match self.spend_open(hash).await {
                    Ok(open) => open,
                    Err(e) => {
                        tracing::warn!(error = %e, auth_hash = %hash, "owed settlement unreadable; kept for the next start");
                        continue;
                    }
                };
            let settled = if open {
                self.submit(&settlement).await
            } else {
                Ok(())
            };
            if let Err(e) = settled.and_then(|()| journal.settled(hash)) {
                tracing::error!(error = %e, auth_hash = %hash, "owed settlement not settled; kept for the next start");
            }
        }
    }

    /// Whether the authorization is on-chain and not yet claimed.
    async fn spend_open(&self, auth_hash: &str) -> anyhow::Result<bool> {
        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());
        let spend = IShieldedCredits::new(self.shielded_credits, &provider)
            .getSpendAuth(auth_hash.parse()?)
            .call()
            .await?;
        Ok(!spend.amount.is_zero() && !spend.claimed)
    }

    pub fn service_id(&self) -> u64 {
        self.service_id
    }

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

    /// Claims the full preauthorization when usage is unavailable.
    /// `actual_amount` is audit-only; metered callers use [`Self::settle_payment`].
    pub async fn claim_payment(
        &self,
        spend_auth: &SpendAuthPayload,
        actual_amount: u64,
    ) -> anyhow::Result<()> {
        self.settle_on_chain(spend_auth, actual_amount, false).await
    }

    /// Charges metered use and atomically refunds unused preauthorization.
    /// `actual_amount` must not exceed the preauthorized amount or the contract reverts.
    pub async fn settle_payment(
        &self,
        spend_auth: &SpendAuthPayload,
        actual_amount: u64,
    ) -> anyhow::Result<()> {
        self.settle_on_chain(spend_auth, actual_amount, true).await
    }

    /// `metered` settles `amount` and refunds the rest; otherwise the full preauthorization is
    /// claimed. The settlement is journaled first, so a killed operator settles it at its next start.
    async fn settle_on_chain(
        &self,
        spend_auth: &SpendAuthPayload,
        amount: u64,
        metered: bool,
    ) -> anyhow::Result<()> {
        let settlement = Settlement {
            auth_hash: Self::auth_hash(spend_auth)?.to_string(),
            spend_auth: spend_auth.clone(),
            amount,
            metered,
        };
        let journal = self.journal.as_deref();
        if let Err(e) = journal.map_or(Ok(()), |j| j.owe(&settlement)) {
            tracing::error!(error = %e, "settlement not journaled — a kill before it confirms loses it");
        }
        self.submit(&settlement).await?;
        if let Err(e) = journal.map_or(Ok(()), |j| j.settled(&settlement.auth_hash)) {
            tracing::warn!(error = %e, "confirmed settlement left in the journal; the next start skips it");
        }
        Ok(())
    }

    /// Send the settlement, retrying `claim_max_retries` times with exponential backoff.
    async fn submit(&self, settlement: &Settlement) -> anyhow::Result<()> {
        let Settlement {
            auth_hash,
            spend_auth,
            amount,
            metered,
        } = settlement;
        let auth_hash: B256 = auth_hash.parse()?;
        let operator: Address = spend_auth.operator.parse()?;
        let max_retries = self.claim_max_retries;
        let op_label = if *metered {
            "settlePayment"
        } else {
            "claimPayment"
        };

        tracing::info!(
            actual_amount = amount,
            preauth_amount = %spend_auth.amount,
            "settling payment on-chain via {op_label}"
        );

        let mut last_err = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retrying {op_label}"
                );
                tokio::time::sleep(delay).await;
            }

            if let Err(e) = self.check_gas_price().await {
                tracing::warn!(error = %e, attempt, "gas price check failed for {op_label}");
                last_err = Some(e);
                continue;
            }

            let provider = ProviderBuilder::new()
                .wallet(self.wallet.clone())
                .connect_http(self.rpc_url.clone());

            let contract = IShieldedCredits::new(self.shielded_credits, &provider);

            let send_result = if *metered {
                contract
                    .settlePayment(auth_hash, operator, U256::from(*amount))
                    .send()
                    .await
            } else {
                contract.claimPayment(auth_hash, operator).send().await
            };

            match send_result {
                Ok(pending) => match pending.get_receipt().await {
                    Ok(receipt) => {
                        tracing::info!(
                            tx_hash = %receipt.transaction_hash,
                            actual_amount = amount,
                            attempt,
                            "{op_label} confirmed"
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

        let err = last_err.unwrap_or_else(|| anyhow::anyhow!("{op_label} failed"));
        tracing::error!(
            error = %err,
            auth_hash = %auth_hash,
            actual_amount = amount,
            commitment = %spend_auth.commitment,
            journaled = self.journal.is_some(),
            "{op_label} FAILED after {max_retries} retries — a journaled settlement is retried at the next start, otherwise the operator served inference for free"
        );
        Err(err)
    }

    /// Return an unsettled authorization's full credit to its account; only its operator may.
    pub(crate) async fn release_payment(&self, spend_auth: &SpendAuthPayload) -> anyhow::Result<()> {
        let provider = ProviderBuilder::new()
            .wallet(self.wallet.clone())
            .connect_http(self.rpc_url.clone());
        let contract = IShieldedCredits::new(self.shielded_credits, &provider);
        let release = contract.releasePayment(Self::auth_hash(spend_auth)?);
        let receipt = release.send().await?.get_receipt().await?;
        tracing::info!(tx_hash = %receipt.transaction_hash, "releasePayment confirmed");
        Ok(())
    }

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

/// Recovers the EIP-712 SpendAuth signer and checks expiry with clock-skew tolerance.
/// Callers must compare it with the account's on-chain spending key before serving.
pub fn recover_spend_auth_signer(
    auth: &SpendAuthPayload,
    shielded_credits_addr: &str,
    chain_id: u64,
    clock_skew_tolerance_secs: u64,
) -> Result<Address, String> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    let (digest, _, _) = spend_auth_hashes(auth, shielded_credits_addr, chain_id)?;

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

fn spend_auth_hashes(
    auth: &SpendAuthPayload,
    shielded_credits_addr: &str,
    chain_id: u64,
) -> Result<(B256, B256, B256), String> {
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

    Ok((digest, domain_separator, struct_hash))
}
