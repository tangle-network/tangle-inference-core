pub mod config;
pub mod health;
pub mod settlement_queue;

#[cfg(feature = "billing")]
pub mod billing;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "billing")]
pub mod payment;
#[cfg(all(feature = "http", feature = "billing"))]
pub mod server;

// Re-exports — gated by feature
#[cfg(feature = "billing")]
pub use billing::{
    recover_spend_auth_signer, verify_spend_auth_signature, BillingClient, CostModel, CostParams,
    FlatRequestCostModel, PerCharCostModel, PerImageCostModel, PerSecondCostModel,
    PerTokenCostModel, TaskTypeCostModel,
};
#[cfg(feature = "metrics")]
pub use metrics::{gather, health_summary, on_chain_metrics, RequestGuard};
#[cfg(feature = "billing")]
pub use payment::{
    create_provider, DirectProvider, NoopProvider, PaymentMode, PaymentProof, PaymentProvider,
    ShieldedProvider,
};
#[cfg(all(feature = "http", feature = "billing"))]
pub use server::{
    acquire_permit, billing_gate, error_response, extract_x402_spend_auth, gpu_health_handler,
    metrics_handler, payment_gate, payment_required, settle_billing, settle_billing_with_recovery,
    settle_payment, validate_spend_auth, AccountGuard, AppState, AppStateBuilder, ErrorDetail,
    ErrorResponse, NonceStore, SpendAuthPayload,
};
pub use settlement_queue::{FailedSettlement, SettlementRecoveryQueue};

// Always available
pub use config::{BillingConfig, GpuConfig, ServerConfig, TangleConfig};
pub use health::{detect_gpus, parse_nvidia_smi_output, tee_status, GpuInfo};
