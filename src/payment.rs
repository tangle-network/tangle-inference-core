//! Payment provider abstraction.
//!
//! Decouples inference billing from any specific payment mechanism.
//! Two implementations:
//!   - `ShieldedProvider` — existing ShieldedCredits flow (authorize → serve → claim)
//!   - `DirectProvider` — verify an ERC-20 transfer on-chain (no shielded pool)
//!
//! Blueprints use `PaymentProvider` trait; config selects the implementation.

use alloy::primitives::Address;
use async_trait::async_trait;

use crate::billing::BillingClient;
use crate::config::{BillingConfig, TangleConfig};
use crate::server::SpendAuthPayload;

/// Payment proof submitted by the caller.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaymentProof {
    /// ShieldedCredits spend authorization (existing path).
    SpendAuth(SpendAuthPayload),
    /// Direct on-chain transfer — caller provides tx hash as proof.
    DirectTransfer {
        tx_hash: String,
        from: String,
        amount: String,
        token: String,
    },
}

/// Result of payment authorization.
#[derive(Debug, Clone)]
pub struct AuthorizationResult {
    pub authorized_amount: u64,
    pub proof: PaymentProof,
}

/// Trait for payment providers. Blueprints depend on this, not on a specific
/// payment mechanism.
#[async_trait]
pub trait PaymentProvider: Send + Sync + 'static {
    /// Verify and authorize a payment proof before serving inference.
    /// Returns the authorized amount on success.
    async fn authorize(&self, proof: &PaymentProof) -> anyhow::Result<u64>;

    /// Settle payment after inference is served.
    /// `actual_cost` may be less than the authorized amount.
    async fn settle(
        &self,
        proof: &PaymentProof,
        actual_cost: u64,
    ) -> anyhow::Result<()>;

    /// Operator's on-chain address.
    fn operator_address(&self) -> Address;
}

// ─── ShieldedProvider ─────────────────────────────────────────────────

/// Existing ShieldedCredits payment path.
/// Wraps `BillingClient` behind the `PaymentProvider` trait.
pub struct ShieldedProvider {
    client: BillingClient,
}

impl ShieldedProvider {
    pub fn new(tangle: &TangleConfig, billing: &BillingConfig) -> anyhow::Result<Self> {
        Ok(Self {
            client: BillingClient::new(tangle, billing)?,
        })
    }

    pub fn client(&self) -> &BillingClient {
        &self.client
    }
}

#[async_trait]
impl PaymentProvider for ShieldedProvider {
    async fn authorize(&self, proof: &PaymentProof) -> anyhow::Result<u64> {
        let PaymentProof::SpendAuth(spend_auth) = proof else {
            anyhow::bail!("ShieldedProvider requires SpendAuth proof, got {:?}", std::mem::discriminant(proof));
        };
        let amount: u64 = spend_auth.amount.parse()?;
        self.client.authorize_spend(spend_auth).await?;
        Ok(amount)
    }

    async fn settle(
        &self,
        proof: &PaymentProof,
        actual_cost: u64,
    ) -> anyhow::Result<()> {
        let PaymentProof::SpendAuth(spend_auth) = proof else {
            anyhow::bail!("ShieldedProvider requires SpendAuth proof for settlement");
        };
        self.client.claim_payment(spend_auth, actual_cost).await
    }

    fn operator_address(&self) -> Address {
        self.client.operator_address()
    }
}

// ─── DirectProvider ───────────────────────────────────────────────────

/// Direct on-chain payment verification.
///
/// Caller makes an ERC-20 `transfer(operator, amount)` tx and includes the
/// tx hash as proof. The provider verifies the receipt on-chain:
///   1. Tx exists and is confirmed
///   2. Recipient matches operator address
///   3. Amount >= requested cost
///   4. Token address matches configured payment token
///
/// No shielded pool, no authorization step, no claim step.
/// Settlement is a no-op (payment already transferred).
pub struct DirectProvider {
    rpc_url: reqwest::Url,
    operator_address: Address,
    /// Required — operators MUST pin a specific ERC-20 token to accept.
    /// Without this, an attacker deploys a worthless token and pays with it.
    expected_token: Address,
    min_confirmations: u64,
    /// Replay protection — tracks used tx hashes. A tx_hash can only
    /// authorize one request. Without this, one real transfer = unlimited
    /// free inference.
    used_tx_hashes: tokio::sync::RwLock<std::collections::HashSet<String>>,
}

impl DirectProvider {
    pub fn new(
        rpc_url: String,
        operator_key: String,
        payment_token: Option<String>,
        min_confirmations: u64,
    ) -> anyhow::Result<Self> {
        use alloy::signers::local::PrivateKeySigner;
        let signer: PrivateKeySigner = operator_key.parse()?;
        let operator_address = signer.address();
        let expected_token: Address = payment_token
            .ok_or_else(|| anyhow::anyhow!(
                "DirectProvider requires payment_token_address — without it, an attacker can pay with a worthless token"
            ))?
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid payment token address: {e}"))?;

        Ok(Self {
            rpc_url: rpc_url.parse()?,
            operator_address,
            expected_token,
            min_confirmations,
            used_tx_hashes: tokio::sync::RwLock::new(std::collections::HashSet::new()),
        })
    }
}

#[async_trait]
impl PaymentProvider for DirectProvider {
    async fn authorize(&self, proof: &PaymentProof) -> anyhow::Result<u64> {
        let PaymentProof::DirectTransfer { tx_hash, from: _, amount, token: _ } = proof else {
            anyhow::bail!("DirectProvider requires DirectTransfer proof, got {:?}", std::mem::discriminant(proof));
        };

        // ── Replay protection: each tx_hash can only authorize ONE request ──
        {
            let mut used = self.used_tx_hashes.write().await;
            if used.contains(tx_hash) {
                anyhow::bail!("tx_hash {tx_hash} already used — replay rejected");
            }
            // Insert BEFORE verification so a concurrent request can't race past
            used.insert(tx_hash.clone());
        }

        use alloy::providers::{Provider, ProviderBuilder};
        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());

        // Parse tx hash
        let hash: alloy::primitives::B256 = tx_hash.parse()
            .map_err(|e| anyhow::anyhow!("invalid tx_hash: {e}"))?;

        // Get receipt
        let receipt = provider
            .get_transaction_receipt(hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("tx {tx_hash} not found — not yet confirmed?"))?;

        // ── Require block_number (reject pending/unconfirmed txs) ──
        let block_number = receipt.block_number
            .ok_or_else(|| anyhow::anyhow!("tx {tx_hash} has no block number — pending or unconfirmed"))?;

        if self.min_confirmations > 0 {
            let current_block = provider.get_block_number().await?;
            let confirmations = current_block.saturating_sub(block_number);
            if confirmations < self.min_confirmations {
                anyhow::bail!(
                    "tx has {confirmations} confirmations, need {min}",
                    min = self.min_confirmations
                );
            }
        }

        // Check status (1 = success)
        if !receipt.status() {
            anyhow::bail!("tx {tx_hash} reverted");
        }

        // ── Verify ERC-20 Transfer to operator with the REQUIRED token ──
        let transfer_topic = alloy::primitives::keccak256(b"Transfer(address,address,uint256)");
        let mut found_amount: u64 = 0;
        let mut found = false;

        for log in receipt.inner.logs() {
            let topics = log.topics();
            if topics.first() != Some(&transfer_topic) {
                continue;
            }
            if topics.len() < 3 {
                continue;
            }

            // topics[2] = recipient (address padded to 32 bytes)
            let recipient = Address::from_slice(&topics[2].as_slice()[12..]);
            if recipient != self.operator_address {
                continue;
            }

            // Token MUST match the configured expected_token (no longer optional)
            if log.address() != self.expected_token {
                continue;
            }

            // data = amount (uint256). Cap at u64::MAX (safe — only inflates,
            // never deflates, so an attacker can't underpay this way).
            let value = alloy::primitives::U256::from_be_slice(log.data().data.as_ref());
            found_amount = value.try_into().unwrap_or(u64::MAX);
            found = true;
            break;
        }

        if !found {
            anyhow::bail!(
                "tx {tx_hash} has no ERC-20 Transfer to operator {} with token {}",
                self.operator_address, self.expected_token
            );
        }

        // ── Parse requested amount strictly — no silent 0 fallback ──
        let requested: u64 = amount.parse()
            .map_err(|e| anyhow::anyhow!("invalid amount in proof: {e}"))?;
        if found_amount < requested {
            anyhow::bail!(
                "transferred amount ({found_amount}) is less than requested ({requested})"
            );
        }

        tracing::info!(
            tx_hash,
            amount = found_amount,
            "direct payment verified"
        );

        Ok(found_amount)
    }

    async fn settle(
        &self,
        _proof: &PaymentProof,
        _actual_cost: u64,
    ) -> anyhow::Result<()> {
        // No-op: payment already transferred directly to operator.
        // If actual_cost < authorized amount, the operator keeps the excess
        // (same as any prepaid model). Could add refund logic later.
        Ok(())
    }

    fn operator_address(&self) -> Address {
        self.operator_address
    }
}

// ─── NoopProvider ─────────────────────────────────────────────────────

/// No billing — open endpoint. `billing_required: false` uses this.
pub struct NoopProvider {
    operator_address: Address,
}

impl NoopProvider {
    pub fn new(operator_key: String) -> anyhow::Result<Self> {
        use alloy::signers::local::PrivateKeySigner;
        let signer: PrivateKeySigner = operator_key.parse()?;
        Ok(Self {
            operator_address: signer.address(),
        })
    }
}

#[async_trait]
impl PaymentProvider for NoopProvider {
    async fn authorize(&self, _proof: &PaymentProof) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn settle(&self, _proof: &PaymentProof, _actual_cost: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn operator_address(&self) -> Address {
        self.operator_address
    }
}

// ─── Factory ──────────────────────────────────────────────────────────

/// Payment mode selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMode {
    /// No billing — open endpoint.
    None,
    /// ShieldedCredits (existing path).
    Shielded,
    /// Direct ERC-20 transfer verification.
    Direct,
}

impl Default for PaymentMode {
    fn default() -> Self {
        Self::Shielded
    }
}

/// Create a payment provider from config.
pub fn create_provider(
    mode: PaymentMode,
    tangle: &TangleConfig,
    billing: &BillingConfig,
) -> anyhow::Result<Box<dyn PaymentProvider>> {
    match mode {
        PaymentMode::None => Ok(Box::new(NoopProvider::new(tangle.operator_key.clone())?)),
        PaymentMode::Shielded => Ok(Box::new(ShieldedProvider::new(tangle, billing)?)),
        PaymentMode::Direct => {
            if billing.payment_token_address.is_none() {
                anyhow::bail!(
                    "payment_mode=direct requires payment_token_address in billing config — \
                     without it, attackers can pay with worthless tokens"
                );
            }
            Ok(Box::new(DirectProvider::new(
                tangle.rpc_url.clone(),
                tangle.operator_key.clone(),
                billing.payment_token_address.clone(),
                1, // min confirmations
            )?))
        }
    }
}
