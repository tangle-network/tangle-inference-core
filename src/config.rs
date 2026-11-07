use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Tangle network connection and identity configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct TangleConfig {
    pub rpc_url: String,
    pub chain_id: u64,
    pub operator_key: String,
    pub shielded_credits: String,
    pub blueprint_id: u64,
    pub service_id: Option<u64>,
}

impl fmt::Debug for TangleConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TangleConfig")
            .field("rpc_url", &self.rpc_url)
            .field("chain_id", &self.chain_id)
            .field("operator_key", &"[REDACTED]")
            .field("shielded_credits", &self.shielded_credits)
            .field("blueprint_id", &self.blueprint_id)
            .field("service_id", &self.service_id)
            .finish()
    }
}

/// AgentProfile secret reference; the SDK provider resolves its public key identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum KeyReference {
    SecretRef {
        key: String,
        #[serde(default)]
        format: Option<SecretFormat>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretFormat {
    Raw,
    Bearer,
}

#[cfg(feature = "http")]
impl KeyReference {
    pub fn secret_bytes(
        &self,
        keystore_root: &std::path::Path,
    ) -> anyhow::Result<zeroize::Zeroizing<Vec<u8>>> {
        use blueprint_keystore::{
            backends::Backend,
            crypto::{k256::K256Ecdsa, BytesEncoding},
            Keystore, KeystoreConfig,
        };
        let path = keystore_root.canonicalize()?;
        let metadata = std::fs::metadata(&path)?;
        anyhow::ensure!(metadata.is_dir(), "SDK keystore root must be a directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "SDK keystore root must be private to its owner"
            );
        }
        let store = Keystore::new(KeystoreConfig::new().fs_root(path))
            .map_err(|_| anyhow::anyhow!("SDK keystore is unavailable"))?;
        let Self::SecretRef { key, .. } = self;
        let selector = key.strip_prefix("0x").unwrap_or(key);
        let public = store
            .list_local::<K256Ecdsa>()
            .map_err(|_| anyhow::anyhow!("SDK public keys are unavailable"))?
            .into_iter()
            .find(|public| public.to_string().eq_ignore_ascii_case(selector))
            .ok_or_else(|| anyhow::anyhow!("selected SDK key is unavailable"))?;
        let secret = store
            .get_secret::<K256Ecdsa>(&public)
            .map_err(|_| anyhow::anyhow!("selected SDK key is unavailable"))?;
        let bytes = zeroize::Zeroizing::new(secret.to_bytes());
        anyhow::ensure!(bytes.len() == 32, "SDK endpoint key must contain 32 bytes");
        Ok(bytes)
    }
}

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub endpoint_key: Option<KeyReference>,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_stream_timeout_secs")]
    pub stream_timeout_secs: u64,
    #[serde(default = "default_idle_chunk_timeout_secs")]
    pub idle_chunk_timeout_secs: u64,
    #[serde(default = "default_max_line_buf_bytes")]
    pub max_line_buf_bytes: usize,
    /// Per-account concurrency; zero means unlimited.
    #[serde(default)]
    pub max_per_account_requests: usize,
}

/// Enabled payment rails. An empty set leaves billing disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentRails {
    #[serde(default)]
    pub shielded: bool,
    #[serde(default)]
    pub direct: bool,
}

impl PaymentRails {
    pub const NONE: Self = Self {
        shielded: false,
        direct: false,
    };
    pub const SHIELDED: Self = Self {
        shielded: true,
        direct: false,
    };
    pub const DIRECT: Self = Self {
        shielded: false,
        direct: true,
    };
    pub const BOTH: Self = Self {
        shielded: true,
        direct: true,
    };

    pub fn is_empty(&self) -> bool {
        !self.shielded && !self.direct
    }
}

impl Default for PaymentRails {
    fn default() -> Self {
        Self::SHIELDED
    }
}

/// Billing configuration rejects stale or misspelled fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingConfig {
    #[serde(default)]
    pub payment_rails: PaymentRails,

    #[serde(default = "default_billing_required")]
    pub billing_required: bool,

    pub max_spend_per_request: u64,

    pub min_credit_balance: u64,

    #[serde(default)]
    pub min_charge_amount: u64,

    #[serde(default = "default_claim_max_retries")]
    pub claim_max_retries: u32,

    /// Receipt wait per operator transaction, default 60 s: many blocks, yet bounded so a never-mined
    /// authorizeSpend is a 402, a settlement a retried attempt, and a release or heartbeat a log line.
    #[serde(default = "default_receipt_timeout_secs")]
    pub receipt_timeout_secs: u64,

    #[serde(default = "default_clock_skew_tolerance")]
    pub clock_skew_tolerance_secs: u64,

    /// Zero disables the gas-price cap.
    #[serde(default)]
    pub max_gas_price_gwei: u64,

    #[serde(default = "default_nonce_store_path")]
    pub nonce_store_path: Option<PathBuf>,

    /// Settlements owed for served work, settled at the next start if the operator is killed first.
    #[serde(default = "default_settlement_journal_path")]
    pub settlement_journal_path: Option<PathBuf>,

    /// Persist consumed transfers to prevent replay after restart.
    #[serde(default = "default_direct_replay_store_path")]
    pub direct_replay_store_path: Option<PathBuf>,

    #[serde(default)]
    pub payment_token_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuConfig {
    pub expected_gpu_count: u32,

    pub min_vram_mib: u32,

    #[serde(default)]
    pub gpu_model: Option<String>,

    #[serde(default = "default_monitor_interval")]
    pub monitor_interval_secs: u64,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_max_concurrent() -> usize {
    64
}

fn default_billing_required() -> bool {
    true
}

fn default_monitor_interval() -> u64 {
    30
}

fn default_max_request_body_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_stream_timeout_secs() -> u64 {
    300
}

fn default_idle_chunk_timeout_secs() -> u64 {
    30
}

fn default_max_line_buf_bytes() -> usize {
    1024 * 1024
}

pub(crate) fn default_claim_max_retries() -> u32 {
    3
}

pub(crate) fn default_receipt_timeout_secs() -> u64 {
    60
}

fn default_clock_skew_tolerance() -> u64 {
    30
}

fn default_nonce_store_path() -> Option<PathBuf> {
    Some(PathBuf::from("data/nonces.json"))
}

fn default_settlement_journal_path() -> Option<PathBuf> {
    Some(PathBuf::from("data/settlements.jsonl"))
}

fn default_direct_replay_store_path() -> Option<PathBuf> {
    Some(PathBuf::from("data/used-tx.json"))
}
