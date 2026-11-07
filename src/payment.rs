//! Shielded authorization and verified direct transfer payment rails.

use std::collections::HashSet;
use std::path::PathBuf;

use alloy::primitives::Address;
use async_trait::async_trait;

use crate::billing::BillingClient;
use crate::config::{BillingConfig, TangleConfig};
use crate::server::SpendAuthPayload;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaymentProof {
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
    /// Rate limit identity: shielded commitment or claimed direct transfer sender.
    pub fn payer_id(&self) -> &str {
        match self {
            PaymentProof::SpendAuth(sa) => &sa.commitment,
            PaymentProof::DirectTransfer { from, .. } => from,
        }
    }
}

/// Verifies payment before inference and settles according to the selected rail.
#[async_trait]
pub trait PaymentProvider: Send + Sync + 'static {
    /// Returns the authorized amount in base token units, refusing proofs below `required`.
    async fn authorize(&self, proof: &PaymentProof, required: u64) -> anyhow::Result<u64>;

    /// Settles metered cost after serving; refund policy belongs to each rail.
    async fn settle(&self, proof: &PaymentProof, actual_cost: u64) -> anyhow::Result<()>;

    fn operator_address(&self) -> Address;
}

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
    async fn authorize(&self, proof: &PaymentProof, _required: u64) -> anyhow::Result<u64> {
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
        self.client.settle_payment(spend_auth, actual_cost).await
    }

    fn operator_address(&self) -> Address {
        self.client.operator_address()
    }
}

/// Atomically reserves each transfer against replay and concurrent requests.
/// Commit persists verified consumption when configured; failed verification releases
/// the reservation for retry. Failed writes are logged and weaken restart protection.
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

    async fn commit(&self, tx_hash: &str) {
        let used_snapshot = {
            let mut g = self.inner.lock().await;
            g.in_flight.remove(tx_hash);
            g.used.insert(tx_hash.to_string());
            g.used.clone()
        };
        self.persist(&used_snapshot);
    }

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
        if let Err(e) = std::fs::write(&tmp, &data) {
            tracing::error!(error = %e, "failed to write direct-transfer tx store \
                — replay protection will not survive a restart");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::error!(error = %e, "failed to persist direct-transfer tx store \
                — replay protection will not survive a restart");
        }
    }
}

/// Requires a successful transfer with sufficient confirmations to this operator,
/// using its pinned ERC-20 token and at least the requested amount.
/// Each transfer is consumed once; prepaid excess is retained at settlement.
pub struct DirectProvider {
    rpc_url: reqwest::Url,
    operator_address: Address,
    expected_token: Address,
    min_confirmations: u64,
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

            // Saturation preserves payment sufficiency for every u64 request.
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
    async fn authorize(&self, proof: &PaymentProof, required: u64) -> anyhow::Result<u64> {
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

        self.replay.reserve(tx_hash).await?;
        match self.verify_transfer(tx_hash, from, amount).await {
            // The verified transfer, not the client's claimed amount, must cover the request.
            Ok(found_amount) if found_amount < required => {
                self.replay.release(tx_hash).await;
                anyhow::bail!("transfer of {found_amount} is below the required {required}")
            }
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
        Ok(())
    }

    fn operator_address(&self) -> Address {
        self.operator_address
    }
}

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
    async fn authorize(&self, _proof: &PaymentProof, _required: u64) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn settle(&self, _proof: &PaymentProof, _actual_cost: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn operator_address(&self) -> Address {
        self.operator_address
    }
}

pub use crate::config::PaymentRails;

/// Enabled rails derive the same operator payout address from the configured key.
/// Disabled proof types are rejected.
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
        if rails.is_empty() {
            anyhow::bail!(
                "PaymentRouter requires at least one enabled rail \
                 (use create_provider for the open/unbilled case)"
            );
        }
        let shielded = rails
            .shielded
            .then(|| ShieldedProvider::new(tangle, billing))
            .transpose()?;
        let direct = rails
            .direct
            .then(|| build_direct(tangle, billing))
            .transpose()?;
        let operator_address = shielded
            .as_ref()
            .map(|p| p.operator_address())
            .or_else(|| direct.as_ref().map(|p| p.operator_address()))
            .expect("non-empty rails guarantees a provider");
        Ok(Self {
            shielded,
            direct,
            operator_address,
        })
    }

    fn provider_for(&self, proof: &PaymentProof) -> anyhow::Result<&dyn PaymentProvider> {
        match proof {
            PaymentProof::SpendAuth(_) => self
                .shielded
                .as_ref()
                .map(|p| p as &dyn PaymentProvider)
                .ok_or_else(|| anyhow::anyhow!("shielded rail not enabled on this operator")),
            PaymentProof::DirectTransfer { .. } => self
                .direct
                .as_ref()
                .map(|p| p as &dyn PaymentProvider)
                .ok_or_else(|| anyhow::anyhow!("direct rail not enabled on this operator")),
        }
    }
}

#[async_trait]
impl PaymentProvider for PaymentRouter {
    async fn authorize(&self, proof: &PaymentProof, required: u64) -> anyhow::Result<u64> {
        self.provider_for(proof)?.authorize(proof, required).await
    }

    async fn settle(&self, proof: &PaymentProof, actual_cost: u64) -> anyhow::Result<()> {
        self.provider_for(proof)?.settle(proof, actual_cost).await
    }

    fn operator_address(&self) -> Address {
        self.operator_address
    }
}

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

/// Selects enabled rails, or [`NoopProvider`] for an empty (unbilled) set.
pub fn create_provider(
    rails: PaymentRails,
    tangle: &TangleConfig,
    billing: &BillingConfig,
) -> anyhow::Result<Box<dyn PaymentProvider>> {
    if rails.is_empty() {
        // Without a rail nothing verifies a proof, so required billing would accept any proof.
        anyhow::ensure!(!billing.billing_required, "billing_required needs an enabled payment rail");
        Ok(Box::new(NoopProvider::new(tangle.operator_key.clone())?))
    } else {
        Ok(Box::new(PaymentRouter::build(rails, tangle, billing)?))
    }
}
