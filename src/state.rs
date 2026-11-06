use crate::billing::BillingClient;
use crate::config::{BillingConfig, ServerConfig, TangleConfig};
use crate::payment::{create_provider, PaymentProvider};
use crate::settlement_queue::SettlementRecoveryQueue;
use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

/// Nonce key: (commitment, nonce) pair for replay protection.
pub type NonceKey = (String, u64);

#[derive(Serialize, Deserialize)]
struct NonceRecord {
    commitment: String,
    nonce: u64,
    expiry: u64,
}

/// Replay-protection nonce store with optional file persistence.
pub struct NonceStore {
    nonces: RwLock<HashMap<NonceKey, u64>>,
    path: Option<PathBuf>,
}

impl NonceStore {
    /// Create a new nonce store, loading persisted nonces from disk if path is set.
    pub fn load(path: Option<PathBuf>) -> Self {
        let nonces: HashMap<NonceKey, u64> = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|data| serde_json::from_str::<Vec<NonceRecord>>(&data).ok())
            .map(|records| {
                records
                    .into_iter()
                    .map(|r| ((r.commitment, r.nonce), r.expiry))
                    .collect()
            })
            .unwrap_or_default();

        if path.is_some() {
            tracing::info!(count = nonces.len(), "loaded persisted nonces");
        } else {
            tracing::warn!(
                "nonce_store_path not configured — nonces are in-memory only. \
                 Operator restart will allow replay of unexpired SpendAuth signatures."
            );
        }

        Self {
            nonces: RwLock::new(nonces),
            path,
        }
    }

    /// Check if the nonce is already used. Does NOT insert — use
    /// `check_and_insert` for the atomic check+insert pattern.
    pub async fn check_replay(&self, key: &NonceKey, tolerance: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut nonces = self.nonces.write().await;
        nonces.retain(|_, expiry| now <= expiry.saturating_add(tolerance));
        nonces.contains_key(key)
    }

    /// Atomically check if a nonce is already used AND insert it if not.
    /// Returns `true` if the nonce was already present (replay detected).
    /// Returns `false` if the nonce was fresh and has been recorded.
    ///
    /// This is the primary API for replay protection — it holds the write
    /// lock across both the check and the insert, preventing TOCTOU races
    /// where two concurrent requests with the same nonce could both pass
    /// a separate check_replay before either inserts.
    pub async fn check_and_insert(&self, key: NonceKey, expiry: u64, tolerance: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut nonces = self.nonces.write().await;
        nonces.retain(|_, exp| now <= exp.saturating_add(tolerance));
        if nonces.contains_key(&key) {
            return true; // replay
        }
        nonces.insert(key, expiry);
        self.persist(&nonces);
        false // fresh
    }

    /// Record a nonce as used, evict expired entries, and persist to disk.
    /// Prefer `check_and_insert` for the atomic check+insert pattern.
    pub async fn insert(&self, key: NonceKey, expiry: u64, tolerance: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut nonces = self.nonces.write().await;
        nonces.retain(|_, exp| now <= exp.saturating_add(tolerance));
        nonces.insert(key, expiry);
        self.persist(&nonces);
    }

    fn persist(&self, nonces: &HashMap<NonceKey, u64>) {
        let Some(ref path) = self.path else { return };
        let records: Vec<NonceRecord> = nonces
            .iter()
            .map(|((commitment, nonce), expiry)| NonceRecord {
                commitment: commitment.clone(),
                nonce: *nonce,
                expiry: *expiry,
            })
            .collect();
        let Ok(data) = serde_json::to_string(&records) else {
            tracing::error!("failed to serialize nonce store");
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        // A dropped write here means a consumed nonce is live in memory but absent
        // from disk — a restart would let that SpendAuth signature be replayed.
        // Surface it at error level instead of swallowing it.
        if let Err(e) = std::fs::write(&tmp, &data) {
            tracing::error!(error = %e, "failed to write nonce store \
                — replay protection will not survive a restart");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::error!(error = %e, "failed to persist nonce store \
                — replay protection will not survive a restart");
        }
    }
}

/// RAII guard that decrements the per-account active request count on drop.
pub struct AccountGuard {
    commitment: String,
    active: Arc<std::sync::Mutex<HashMap<String, usize>>>,
}

impl AccountGuard {
    /// Create a guard that increments the per-account counter NOW and
    /// decrements it on drop. This ensures the counter is always balanced
    /// regardless of early returns or panics in the caller.
    pub fn acquire(
        commitment: String,
        active: Arc<std::sync::Mutex<HashMap<String, usize>>>,
        max_per_account: usize,
    ) -> Option<Self> {
        let mut map = active.lock().unwrap_or_else(|e| e.into_inner());
        let count = map.entry(commitment.clone()).or_insert(0);
        if max_per_account > 0 && *count >= max_per_account {
            return None; // at capacity
        }
        *count += 1;
        drop(map);
        Some(Self { commitment, active })
    }

    /// Create a guard without checking capacity (for backwards compat).
    pub fn new(commitment: String, active: Arc<std::sync::Mutex<HashMap<String, usize>>>) -> Self {
        Self { commitment, active }
    }
}

impl Drop for AccountGuard {
    fn drop(&mut self) {
        // std::sync::Mutex works in Drop — no async, no spawned task,
        // no race, no lost decrements.
        let mut map = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(&self.commitment) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.commitment);
            }
        }
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub billing: Arc<BillingClient>,
    pub nonce_store: Arc<NonceStore>,
    pub server_config: Arc<ServerConfig>,
    pub billing_config: Arc<BillingConfig>,
    pub tangle_config: Arc<TangleConfig>,
    pub operator_address: Address,
    pub semaphore: Arc<tokio::sync::Semaphore>,
    pub active_per_account: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    /// Persistent dead-letter queue for failed on-chain settlements.
    pub settlement_recovery_queue: Option<Arc<SettlementRecoveryQueue>>,
    /// Backend-specific state. Downcast via [`AppState::backend`].
    pub backend: Arc<dyn std::any::Any + Send + Sync>,
    /// Generic payment provider (replaces direct BillingClient usage).
    /// Supports ShieldedCredits, DirectTransfer, or NoOp depending on config.
    /// New blueprints should use this instead of `billing` directly.
    pub payment_provider: Arc<dyn PaymentProvider>,
}

impl AppState {
    /// Downcast the backend extension to a concrete type.
    ///
    /// Returns `None` if the stored backend is not of type `B`.
    pub fn backend<B: 'static>(&self) -> Option<&B> {
        self.backend.downcast_ref::<B>()
    }

    pub fn from_config<B: Send + Sync + 'static>(
        tangle: &TangleConfig,
        server: &ServerConfig,
        billing: &BillingConfig,
        max_concurrent: usize,
        backend: B,
    ) -> anyhow::Result<Self> {
        let billing_client = BillingClient::new(tangle, billing)?;
        let operator_address = billing_client.operator_address();
        let nonce_store = Arc::new(NonceStore::load(billing.nonce_store_path.clone()));

        // Build the payment provider for the operator's enabled rails.
        let provider = create_provider(billing.payment_rails, tangle, billing)?;

        AppStateBuilder::new()
            .billing(Arc::new(billing_client))
            .nonce_store(nonce_store)
            .server_config(Arc::new(server.clone()))
            .billing_config(Arc::new(billing.clone()))
            .tangle_config(Arc::new(tangle.clone()))
            .operator_address(operator_address)
            .max_concurrent(max_concurrent)
            .payment_provider(Arc::from(provider) as Arc<dyn PaymentProvider>)
            .backend(backend)
            .build()
    }
}

/// Builder for [`AppState`]. Required fields: `billing`, `nonce_store`,
/// `server_config`, `billing_config`, `tangle_config`, `operator_address`,
/// `backend`. `max_concurrent` defaults to `server_config.max_concurrent_requests`.
#[derive(Default)]
pub struct AppStateBuilder {
    billing: Option<Arc<BillingClient>>,
    nonce_store: Option<Arc<NonceStore>>,
    server_config: Option<Arc<ServerConfig>>,
    billing_config: Option<Arc<BillingConfig>>,
    tangle_config: Option<Arc<TangleConfig>>,
    operator_address: Option<Address>,
    max_concurrent: Option<usize>,
    settlement_recovery_queue: Option<Arc<SettlementRecoveryQueue>>,
    payment_provider: Option<Arc<dyn PaymentProvider>>,
    backend: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl AppStateBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn billing(mut self, b: Arc<BillingClient>) -> Self {
        self.billing = Some(b);
        self
    }

    pub fn nonce_store(mut self, n: Arc<NonceStore>) -> Self {
        self.nonce_store = Some(n);
        self
    }

    pub fn server_config(mut self, c: Arc<ServerConfig>) -> Self {
        self.server_config = Some(c);
        self
    }

    pub fn billing_config(mut self, c: Arc<BillingConfig>) -> Self {
        self.billing_config = Some(c);
        self
    }

    pub fn tangle_config(mut self, c: Arc<TangleConfig>) -> Self {
        self.tangle_config = Some(c);
        self
    }

    pub fn operator_address(mut self, a: Address) -> Self {
        self.operator_address = Some(a);
        self
    }

    pub fn max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = Some(n);
        self
    }

    pub fn settlement_recovery_queue(mut self, q: Arc<SettlementRecoveryQueue>) -> Self {
        self.settlement_recovery_queue = Some(q);
        self
    }

    pub fn payment_provider(mut self, p: Arc<dyn PaymentProvider>) -> Self {
        self.payment_provider = Some(p);
        self
    }

    /// Attach a backend value of any type. Retrieve in handlers via
    /// `state.backend::<MyBackend>().unwrap()`.
    pub fn backend<B: Send + Sync + 'static>(mut self, b: B) -> Self {
        self.backend = Some(Arc::new(b));
        self
    }

    pub fn build(self) -> anyhow::Result<AppState> {
        let billing = self
            .billing
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: billing is required"))?;
        let nonce_store = self
            .nonce_store
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: nonce_store is required"))?;
        let server_config = self
            .server_config
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: server_config is required"))?;
        let billing_config = self
            .billing_config
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: billing_config is required"))?;
        let tangle_config = self
            .tangle_config
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: tangle_config is required"))?;
        let operator_address = self
            .operator_address
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: operator_address is required"))?;
        let backend = self
            .backend
            .ok_or_else(|| anyhow::anyhow!("AppStateBuilder: backend is required"))?;

        let max_concurrent = self
            .max_concurrent
            .unwrap_or(server_config.max_concurrent_requests);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

        let payment_provider = match self.payment_provider {
            Some(p) => p,
            None => Arc::from(create_provider(
                billing_config.payment_rails,
                &tangle_config,
                &billing_config,
            )?),
        };

        Ok(AppState {
            billing,
            nonce_store,
            server_config,
            billing_config,
            tangle_config,
            operator_address,
            semaphore,
            active_per_account: Arc::new(std::sync::Mutex::new(HashMap::new())),
            settlement_recovery_queue: self.settlement_recovery_queue,
            payment_provider,
            backend,
        })
    }
}
