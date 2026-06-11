//! Payment provider abstraction.
//!
//! Decouples inference billing from any specific payment mechanism.
//! Two implementations:
//!   - `ShieldedProvider` — existing ShieldedCredits flow (authorize → serve → claim)
//!   - `DirectProvider` — verify an ERC-20 transfer on-chain (no shielded pool)
//!
//! Blueprints use `PaymentProvider` trait; config selects the implementation.

use std::collections::HashSet;
use std::path::PathBuf;

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

impl PaymentProof {
    /// Stable identity of the payer, for per-account rate limiting across rails:
    /// the shielded commitment, or the direct transfer's sender address.
    pub fn payer_id(&self) -> &str {
        match self {
            PaymentProof::SpendAuth(sa) => &sa.commitment,
            PaymentProof::DirectTransfer { from, .. } => from,
        }
    }
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
    async fn settle(&self, proof: &PaymentProof, actual_cost: u64) -> anyhow::Result<()>;

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
            anyhow::bail!(
                "ShieldedProvider requires SpendAuth proof, got {:?}",
                std::mem::discriminant(proof)
            );
        };
        let amount: u64 = spend_auth.amount.parse()?;
        self.client.authorize_spend(spend_auth).await?;
        Ok(amount)
    }

    async fn settle(&self, proof: &PaymentProof, actual_cost: u64) -> anyhow::Result<()> {
        let PaymentProof::SpendAuth(spend_auth) = proof else {
            anyhow::bail!("ShieldedProvider requires SpendAuth proof for settlement");
        };
        self.client.claim_payment(spend_auth, actual_cost).await
    }

    fn operator_address(&self) -> Address {
        self.client.operator_address()
    }
}

// ─── UsedTxStore ──────────────────────────────────────────────────────

/// Replay protection for the Direct rail: a confirmed-consumed set of payment
/// tx hashes, persisted across restarts, plus an ephemeral in-flight set.
///
/// Three properties, all required for a payment rail:
///   1. **Persistent** — a consumed tx hash survives operator restart (the
///      in-memory-only predecessor let a restart replay any past payment for
///      free inference). Mirrors `NonceStore`'s persistence.
///   2. **Concurrency-safe** — `reserve` holds the lock across check+insert, so
///      two concurrent requests with the same tx hash cannot both pass.
///   3. **Retry-safe** — a tx is only *committed* (persisted as consumed) after
///      its on-chain transfer verifies; a transient RPC failure `release`s the
///      reservation so a legitimate payer can retry. The predecessor inserted
///      before verifying, permanently burning a tx on any transient error.
pub struct UsedTxStore {
    inner: tokio::sync::Mutex<UsedTxInner>,
    path: Option<PathBuf>,
}

#[derive(Default)]
struct UsedTxInner {
    used: HashSet<String>,
    in_flight: HashSet<String>,
}

impl UsedTxStore {
    /// Load the consumed set from disk if a path is configured.
    pub fn load(path: Option<PathBuf>) -> Self {
        let used: HashSet<String> = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|data| serde_json::from_str::<Vec<String>>(&data).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();

        if path.is_some() {
            tracing::info!(
                count = used.len(),
                "loaded persisted direct-transfer tx hashes"
            );
        } else {
            tracing::warn!(
                "direct_replay_store_path not configured — used tx hashes are in-memory only. \
                 Operator restart will allow replay of past payment transfers for free inference."
            );
        }

        Self {
            inner: tokio::sync::Mutex::new(UsedTxInner {
                used,
                in_flight: HashSet::new(),
            }),
            path,
        }
    }

    /// Atomically reserve a tx hash for verification. Errors if it was already
    /// consumed (replay) or is already being verified concurrently.
    async fn reserve(&self, tx_hash: &str) -> anyhow::Result<()> {
        let mut g = self.inner.lock().await;
        if g.used.contains(tx_hash) {
            anyhow::bail!("tx_hash {tx_hash} already used — replay rejected");
        }
        if !g.in_flight.insert(tx_hash.to_string()) {
            anyhow::bail!("tx_hash {tx_hash} verification already in progress");
        }
        Ok(())
    }

    /// Commit a verified tx hash to the persistent consumed set.
    async fn commit(&self, tx_hash: &str) {
        let used_snapshot = {
            let mut g = self.inner.lock().await;
            g.in_flight.remove(tx_hash);
            g.used.insert(tx_hash.to_string());
            g.used.clone()
        };
        self.persist(&used_snapshot);
    }

    /// Release a reservation whose verification did not succeed, so a
    /// legitimate payer can retry (e.g. after a transient RPC error).
    async fn release(&self, tx_hash: &str) {
        self.inner.lock().await.in_flight.remove(tx_hash);
    }

    fn persist(&self, used: &HashSet<String>) {
        let Some(ref path) = self.path else { return };
        let records: Vec<&String> = used.iter().collect();
        let Ok(data) = serde_json::to_string(&records) else {
            tracing::error!("failed to serialize direct-transfer tx store");
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &data).is_ok() {
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!(error = %e, "failed to persist direct-transfer tx store");
            }
        }
    }

    #[cfg(test)]
    async fn is_used(&self, tx_hash: &str) -> bool {
        self.inner.lock().await.used.contains(tx_hash)
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
    /// Replay protection — persistent across restarts, retry-safe on transient
    /// failure. A tx_hash authorizes exactly one request, ever.
    replay: UsedTxStore,
}

impl DirectProvider {
    pub fn new(
        rpc_url: String,
        operator_key: String,
        payment_token: Option<String>,
        min_confirmations: u64,
        replay_store_path: Option<PathBuf>,
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
            replay: UsedTxStore::load(replay_store_path),
        })
    }

    /// Verify the on-chain ERC-20 transfer named by the proof. Pure read — does
    /// no replay bookkeeping (the caller reserves before and commits after), so
    /// a transient failure here never burns a legitimate tx hash.
    async fn verify_transfer(
        &self,
        tx_hash: &str,
        from: &str,
        amount: &str,
    ) -> anyhow::Result<u64> {
        use alloy::providers::{Provider, ProviderBuilder};
        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());

        let hash: alloy::primitives::B256 = tx_hash
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid tx_hash: {e}"))?;

        let receipt = provider
            .get_transaction_receipt(hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("tx {tx_hash} not found — not yet confirmed?"))?;

        let block_number = receipt.block_number.ok_or_else(|| {
            anyhow::anyhow!("tx {tx_hash} has no block number — pending or unconfirmed")
        })?;

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

            // Token MUST match the configured expected_token.
            if log.address() != self.expected_token {
                continue;
            }

            // Log (don't reject) a 'from' mismatch — the operator was still paid.
            if !from.is_empty() {
                if let Ok(claimed_from) = from.parse::<Address>() {
                    let actual_from = Address::from_slice(&topics[1].as_slice()[12..]);
                    if actual_from != claimed_from {
                        tracing::warn!(
                            tx_hash,
                            claimed_from = %claimed_from,
                            actual_from = %actual_from,
                            "DirectTransfer 'from' mismatch — claimed sender differs from actual Transfer sender"
                        );
                    }
                }
            }

            // data = amount (uint256). Cap at u64::MAX (only inflates, never
            // deflates — an attacker can't underpay this way).
            let value = alloy::primitives::U256::from_be_slice(log.data().data.as_ref());
            found_amount = value.try_into().unwrap_or(u64::MAX);
            found = true;
            break;
        }

        if !found {
            anyhow::bail!(
                "tx {tx_hash} has no ERC-20 Transfer to operator {} with token {}",
                self.operator_address,
                self.expected_token
            );
        }

        let requested: u64 = amount
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid amount in proof: {e}"))?;
        if found_amount < requested {
            anyhow::bail!(
                "transferred amount ({found_amount}) is less than requested ({requested})"
            );
        }

        Ok(found_amount)
    }
}

#[async_trait]
impl PaymentProvider for DirectProvider {
    async fn authorize(&self, proof: &PaymentProof) -> anyhow::Result<u64> {
        let PaymentProof::DirectTransfer {
            tx_hash,
            from,
            amount,
            token: _,
        } = proof
        else {
            anyhow::bail!(
                "DirectProvider requires DirectTransfer proof, got {:?}",
                std::mem::discriminant(proof)
            );
        };

        // Reserve the tx hash (rejects replay + concurrent use), then verify
        // on-chain. Commit only on success so a transient RPC failure releases
        // the reservation and a legitimate payer can retry.
        self.replay.reserve(tx_hash).await?;
        match self.verify_transfer(tx_hash, from, amount).await {
            Ok(found_amount) => {
                self.replay.commit(tx_hash).await;
                tracing::info!(tx_hash, amount = found_amount, "direct payment verified");
                Ok(found_amount)
            }
            Err(e) => {
                self.replay.release(tx_hash).await;
                Err(e)
            }
        }
    }

    async fn settle(&self, _proof: &PaymentProof, _actual_cost: u64) -> anyhow::Result<()> {
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

// ─── PaymentRouter ────────────────────────────────────────────────────

pub use crate::config::PaymentRails;

/// The universal payment provider: holds the operator's enabled rails and
/// dispatches each request to the matching one by proof type. Enabling several
/// rails lets one endpoint accept any of them; a proof for a disabled rail is
/// rejected. Every rail derives its payout from the same operator key, so funds
/// land in the same place regardless of how the buyer paid.
///
/// This is the single, extensible composition point — adding a rail is a new
/// `Option<NewProvider>` field plus a match arm, never an enum cross-product.
pub struct PaymentRouter {
    shielded: Option<ShieldedProvider>,
    direct: Option<DirectProvider>,
    operator_address: Address,
}

impl PaymentRouter {
    pub fn build(
        rails: PaymentRails,
        tangle: &TangleConfig,
        billing: &BillingConfig,
    ) -> anyhow::Result<Self> {
        use alloy::signers::local::PrivateKeySigner;
        let operator_address = tangle.operator_key.parse::<PrivateKeySigner>()?.address();
        Ok(Self {
            shielded: rails
                .shielded
                .then(|| ShieldedProvider::new(tangle, billing))
                .transpose()?,
            direct: rails
                .direct
                .then(|| build_direct(tangle, billing))
                .transpose()?,
            operator_address,
        })
    }
}

#[async_trait]
impl PaymentProvider for PaymentRouter {
    async fn authorize(&self, proof: &PaymentProof) -> anyhow::Result<u64> {
        match proof {
            PaymentProof::SpendAuth(_) => {
                self.shielded
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("shielded rail not enabled on this operator"))?
                    .authorize(proof)
                    .await
            }
            PaymentProof::DirectTransfer { .. } => {
                self.direct
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("direct rail not enabled on this operator"))?
                    .authorize(proof)
                    .await
            }
        }
    }

    async fn settle(&self, proof: &PaymentProof, actual_cost: u64) -> anyhow::Result<()> {
        match proof {
            PaymentProof::SpendAuth(_) => {
                self.shielded
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("shielded rail not enabled on this operator"))?
                    .settle(proof, actual_cost)
                    .await
            }
            PaymentProof::DirectTransfer { .. } => {
                self.direct
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("direct rail not enabled on this operator"))?
                    .settle(proof, actual_cost)
                    .await
            }
        }
    }

    fn operator_address(&self) -> Address {
        self.operator_address
    }
}

// ─── Factory ──────────────────────────────────────────────────────────

fn build_direct(tangle: &TangleConfig, billing: &BillingConfig) -> anyhow::Result<DirectProvider> {
    if billing.payment_token_address.is_none() {
        anyhow::bail!(
            "direct rail requires payment_token_address in billing config — \
             without it, attackers can pay with worthless tokens"
        );
    }
    DirectProvider::new(
        tangle.rpc_url.clone(),
        tangle.operator_key.clone(),
        billing.payment_token_address.clone(),
        1, // min confirmations
        billing.direct_replay_store_path.clone(),
    )
}

/// Build the operator's payment provider for the rails it accepts. An empty rail
/// set is an open (unbilled) endpoint.
pub fn create_provider(
    rails: PaymentRails,
    tangle: &TangleConfig,
    billing: &BillingConfig,
) -> anyhow::Result<Box<dyn PaymentProvider>> {
    if rails.is_empty() {
        Ok(Box::new(NoopProvider::new(tangle.operator_key.clone())?))
    } else {
        Ok(Box::new(PaymentRouter::build(rails, tangle, billing)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(n: u8) -> String {
        format!("0x{:064x}", n)
    }

    #[tokio::test]
    async fn reserve_rejects_replay_and_concurrent() {
        let store = UsedTxStore::load(None);
        // First reserve succeeds.
        store.reserve(&tx(1)).await.unwrap();
        // Same tx again while in-flight → rejected.
        assert!(
            store.reserve(&tx(1)).await.is_err(),
            "concurrent reserve must fail"
        );
        // Commit it; now it's consumed.
        store.commit(&tx(1)).await;
        assert!(store.is_used(&tx(1)).await);
        // A reserve of a consumed tx → rejected as replay.
        assert!(store.reserve(&tx(1)).await.is_err(), "replay must fail");
    }

    #[tokio::test]
    async fn release_allows_retry_after_transient_failure() {
        let store = UsedTxStore::load(None);
        store.reserve(&tx(2)).await.unwrap();
        // Verification "failed" → release the reservation.
        store.release(&tx(2)).await;
        assert!(
            !store.is_used(&tx(2)).await,
            "released tx must not be consumed"
        );
        // The legitimate payer can retry.
        store.reserve(&tx(2)).await.unwrap();
        store.commit(&tx(2)).await;
        assert!(store.is_used(&tx(2)).await);
    }

    #[tokio::test]
    async fn consumed_tx_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("used-tx.json");

        // Operator run 1: consume a payment tx.
        {
            let store = UsedTxStore::load(Some(path.clone()));
            store.reserve(&tx(3)).await.unwrap();
            store.commit(&tx(3)).await;
        }
        // Operator restart: a fresh store loads the persisted set and STILL
        // rejects the same tx — the bug this fixes (in-memory store forgot it).
        {
            let store = UsedTxStore::load(Some(path.clone()));
            assert!(
                store.is_used(&tx(3)).await,
                "consumed tx must persist across restart"
            );
            assert!(
                store.reserve(&tx(3)).await.is_err(),
                "replay of a past payment after restart must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn released_tx_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("used-tx.json");
        {
            let store = UsedTxStore::load(Some(path.clone()));
            store.reserve(&tx(4)).await.unwrap();
            store.release(&tx(4)).await; // transient failure
        }
        // After restart, a released (never-verified) tx is still spendable.
        let store = UsedTxStore::load(Some(path));
        assert!(!store.is_used(&tx(4)).await);
        assert!(
            store.reserve(&tx(4)).await.is_ok(),
            "a never-committed tx must remain usable"
        );
    }
}
