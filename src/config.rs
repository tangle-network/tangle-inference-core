use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Tangle network connection and identity configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct TangleConfig {
    /// JSON-RPC endpoint for the Tangle EVM chain.
    pub rpc_url: String,

    /// Chain ID.
    pub chain_id: u64,

    /// Operator's private key (hex, without 0x prefix).
    /// In production, use a KMS or hardware signer instead.
    pub operator_key: String,

    /// ShieldedCredits contract address.
    pub shielded_credits: String,

    /// Blueprint ID this operator is registered for.
    pub blueprint_id: u64,

    /// Service ID (set after service activation).
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

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// External host to bind.
    #[serde(default = "default_host")]
    pub host: String,

    /// External port to bind.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Maximum concurrent requests.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,

    /// Maximum request body size in bytes (default 16 MiB).
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,

    /// Per-request timeout for streaming connections in seconds (default 300).
    #[serde(default = "default_stream_timeout_secs")]
    pub stream_timeout_secs: u64,

    /// Per-chunk idle timeout in seconds for streaming responses (default 30).
    #[serde(default = "default_idle_chunk_timeout_secs")]
    pub idle_chunk_timeout_secs: u64,

    /// Maximum size of the SSE line buffer in bytes (default 1 MiB).
    #[serde(default = "default_max_line_buf_bytes")]
    pub max_line_buf_bytes: usize,

    /// Maximum concurrent requests per credit account (commitment).
    /// 0 = unlimited (default).
    #[serde(default)]
    pub max_per_account_requests: usize,
}

/// The payment rails an operator accepts on its billable surfaces.
///
/// A request is served when it carries a valid proof for an ENABLED rail; an
/// empty set is an open (unbilled) endpoint. Rails compose freely — enabling
/// several lets one endpoint take any of them, dispatched per request by proof
/// type (see [`crate::payment::PaymentRouter`]). Adding a new rail is one more
/// flag here plus a `PaymentProvider` impl — never an enum cross-product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRails {
    /// ShieldedCredits SpendAuth — private, prepaid shielded-pool balance.
    #[serde(default)]
    pub shielded: bool,
    /// Direct ERC-20 transfer — plain USDC, pay-per-call (needs a pinned
    /// `payment_token_address`).
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

    /// No rail enabled → open, unbilled endpoint.
    pub fn is_empty(&self) -> bool {
        !self.shielded && !self.direct
    }
}

impl Default for PaymentRails {
    fn default() -> Self {
        Self::SHIELDED
    }
}

/// Billing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingConfig {
    /// Which payment rails this operator accepts. Default: shielded only.
    /// e.g. `{ shielded = true, direct = true }` to take both.
    #[serde(default)]
    pub payment_rails: PaymentRails,

    /// Whether billing (spend_auth / direct transfer) is required on every request.
    #[serde(default = "default_billing_required")]
    pub billing_required: bool,

    /// Maximum amount a single SpendAuth can authorize (anti-abuse).
    pub max_spend_per_request: u64,

    /// Minimum balance required in a credit account to serve a request.
    pub min_credit_balance: u64,

    /// Minimum charge amount per request (gas cost protection).
    #[serde(default)]
    pub min_charge_amount: u64,

    /// Maximum retries for claim_payment on-chain calls.
    #[serde(default = "default_claim_max_retries")]
    pub claim_max_retries: u32,

    /// Clock skew tolerance in seconds for SpendAuth expiry checks.
    #[serde(default = "default_clock_skew_tolerance")]
    pub clock_skew_tolerance_secs: u64,

    /// Maximum gas price in gwei the operator is willing to pay for billing txs.
    /// 0 = no cap (default).
    #[serde(default)]
    pub max_gas_price_gwei: u64,

    /// Path to persist used nonces across restarts (replay protection).
    #[serde(default = "default_nonce_store_path")]
    pub nonce_store_path: Option<PathBuf>,

    /// Path to persist consumed direct-transfer tx hashes across restarts.
    /// Without it, an operator restart forgets used payment txs and the same
    /// transfer can be replayed for unlimited free inference — the Direct
    /// rail's analogue of `nonce_store_path`.
    #[serde(default = "default_direct_replay_store_path")]
    pub direct_replay_store_path: Option<PathBuf>,

    /// ERC-20 token address for x402 payment (e.g. USDC wrapped via VAnchor).
    #[serde(default)]
    pub payment_token_address: Option<String>,
}

/// GPU hardware configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuConfig {
    /// Expected number of GPUs.
    pub expected_gpu_count: u32,

    /// Minimum required VRAM per GPU in MiB.
    pub min_vram_mib: u32,

    /// GPU model name for on-chain registration.
    #[serde(default)]
    pub gpu_model: Option<String>,

    /// GPU monitoring interval in seconds.
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

fn default_clock_skew_tolerance() -> u64 {
    30
}

fn default_nonce_store_path() -> Option<PathBuf> {
    Some(PathBuf::from("data/nonces.json"))
}

fn default_direct_replay_store_path() -> Option<PathBuf> {
    Some(PathBuf::from("data/used-tx.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_defaults() {
        let json = r#"{}"#;
        let cfg: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.max_concurrent_requests, 64);
        assert_eq!(cfg.max_request_body_bytes, 16 * 1024 * 1024);
        assert_eq!(cfg.stream_timeout_secs, 300);
        assert_eq!(cfg.idle_chunk_timeout_secs, 30);
        assert_eq!(cfg.max_line_buf_bytes, 1024 * 1024);
        assert_eq!(cfg.max_per_account_requests, 0);
    }

    #[test]
    fn test_tangle_config_redacts_key() {
        let cfg = TangleConfig {
            rpc_url: "http://localhost:8545".into(),
            chain_id: 31337,
            operator_key: "deadbeef".into(),
            shielded_credits: "0x0000000000000000000000000000000000000002".into(),
            blueprint_id: 1,
            service_id: None,
        };
        let debug = format!("{:?}", cfg);
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("deadbeef"));
    }

    #[test]
    fn test_billing_config_defaults() {
        let json = r#"{
            "max_spend_per_request": 1000000,
            "min_credit_balance": 1000
        }"#;
        let cfg: BillingConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.billing_required);
        assert_eq!(cfg.claim_max_retries, 3);
        assert_eq!(cfg.clock_skew_tolerance_secs, 30);
        assert_eq!(cfg.max_gas_price_gwei, 0);
    }
}
