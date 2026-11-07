use std::time::Duration;

use alloy::{
    network::EthereumWallet,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};

use crate::{config::TangleConfig, metrics};

sol! {
    #[sol(rpc)]
    interface IOperatorStatusRegistry {
        struct MetricPair {
            string key;
            uint64 value;
        }

        function submitHeartbeat(
            uint64 serviceId,
            uint64 blueprintId,
            uint64 blockNumber,
            MetricPair[] calldata metrics
        ) external;
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct QoSConfig {
    #[serde(default)]
    pub heartbeat_interval_secs: u64,

    #[serde(default)]
    pub status_registry_address: Option<String>,
}

pub async fn start_heartbeat(
    qos: &QoSConfig,
    config: &TangleConfig,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let interval_secs = qos.heartbeat_interval_secs;
    if interval_secs == 0 {
        anyhow::bail!("heartbeat disabled (interval = 0)");
    }

    let registry_addr: Address = qos
        .status_registry_address
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("status_registry_address not configured"))?
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid status_registry_address: {e}"))?;

    let signer: PrivateKeySigner = config.operator_key.parse()?;
    let wallet = EthereumWallet::from(signer);
    let rpc_url: reqwest::Url = config.rpc_url.parse()?;
    let service_id = config
        .service_id
        .ok_or_else(|| anyhow::anyhow!("service_id required for QoS heartbeat"))?;
    let blueprint_id = config.blueprint_id;

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.tick().await;

        loop {
            interval.tick().await;

            match send_heartbeat(&wallet, &rpc_url, registry_addr, service_id, blueprint_id).await {
                Ok(()) => {
                    metrics::HEARTBEATS_SENT.inc();
                    tracing::debug!(service_id, blueprint_id, "heartbeat submitted");
                }
                Err(e) => {
                    metrics::HEARTBEATS_FAILED.inc();
                    tracing::warn!(
                        error = %e,
                        service_id,
                        blueprint_id,
                        "heartbeat submission failed"
                    );
                }
            }
        }
    });

    Ok(handle)
}

async fn send_heartbeat(
    wallet: &EthereumWallet,
    rpc_url: &reqwest::Url,
    registry_addr: Address,
    service_id: u64,
    blueprint_id: u64,
) -> anyhow::Result<()> {
    let provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect_http(rpc_url.clone());

    let block_number = provider.get_block_number().await?;

    let chain_metrics = metrics::on_chain_metrics();
    let metric_pairs: Vec<IOperatorStatusRegistry::MetricPair> = chain_metrics
        .into_iter()
        .map(|(key, value)| IOperatorStatusRegistry::MetricPair { key, value })
        .collect();

    let registry = IOperatorStatusRegistry::new(registry_addr, &provider);
    let call = registry.submitHeartbeat(service_id, blueprint_id, block_number, metric_pairs);

    let timeout = Duration::from_secs(crate::config::default_receipt_timeout_secs());
    crate::billing::transact(timeout, call).await?;

    Ok(())
}
