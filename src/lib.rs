pub mod config;
pub mod engine;
pub mod health;
pub mod operator;
pub mod recipe;
pub mod runtime;
pub mod weights;

#[cfg(all(feature = "billing", feature = "metrics"))]
pub mod qos;

#[cfg(feature = "billing")]
pub mod billing;
#[cfg(feature = "billing")]
pub mod journal;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "billing")]
pub mod payment;
#[cfg(all(feature = "http", feature = "billing"))]
pub mod server;
#[cfg(all(feature = "http", feature = "billing"))]
pub mod state;

// Re-exports — gated by feature
#[cfg(feature = "billing")]
pub use billing::{
    recover_spend_auth_signer, BillingClient, CostModel, CostParams, FlatRequestCostModel,
    PerCharCostModel, PerImageCostModel, PerSecondCostModel, PerTokenCostModel, TaskTypeCostModel,
};
#[cfg(feature = "metrics")]
pub use metrics::{gather, health_summary, on_chain_metrics, RequestGuard};
#[cfg(feature = "billing")]
pub use payment::{
    create_provider, DirectProvider, NoopProvider, PaymentProof, PaymentProvider, PaymentRails,
    PaymentRouter, ShieldedProvider,
};
#[cfg(all(feature = "http", feature = "billing"))]
pub use server::{
    acquire_permit, billing_gate, error_response, extract_x402_spend_auth, gpu_health_handler,
    metrics_handler, payment_required, settle_billing, validate_spend_auth, AccountGuard, AppState,
    AppStateBuilder, ErrorDetail, ErrorResponse, NonceStore, SpendAuthPayload,
};

// Always available
pub use config::{BillingConfig, GpuConfig, ServerConfig, TangleConfig};
pub use health::{detect_gpus, parse_nvidia_smi_output, tee_status, GpuInfo};
