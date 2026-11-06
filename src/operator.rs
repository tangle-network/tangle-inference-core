//! Shared blueprint entrypoint. Model adapters supply registration and serving.

/// Expand the SDK runner in the consumer so core does not depend on the SDK.
#[macro_export]
macro_rules! run_operator {
    ($config:expr, $registration:expr, $router:expr, $service:expr, $qos:ident) => {
        #[tokio::main]
        #[allow(clippy::result_large_err)]
        async fn main() -> Result<(), blueprint_sdk::Error> {
            use blueprint_sdk::contexts::tangle::TangleClientContext;
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            tracing_subscriber::fmt().with_env_filter(filter).init();
            dotenvy::dotenv().ok();
            let config = std::sync::Arc::new(($config)
                .map_err(|e| blueprint_sdk::Error::Other(format!("config: {e}")))?);
            let env = blueprint_sdk::runner::config::BlueprintEnvironment::load()?;
            if env.registration_mode() {
                let path = env.registration_output_path();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;
                }
                std::fs::write(&path, ($registration)(&config))
                    .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;
                return Ok(());
            }
            let client = env.tangle_client().await
                .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;
            let service_id = env.protocol_settings.tangle()
                .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?
                .service_id
                .ok_or_else(|| blueprint_sdk::Error::Other("No service ID configured".into()))?;
            $crate::run_operator!(@heartbeat config, $qos);
            blueprint_sdk::runner::BlueprintRunner::builder(
                blueprint_sdk::runner::tangle::config::TangleConfig::default(), env)
                .router(($router)(config.clone()))
                .producer(blueprint_sdk::tangle::TangleProducer::new(client.clone(), service_id))
                .consumer(blueprint_sdk::tangle::TangleConsumer::new(client))
                .background_service(($service)(config))
                .run().await?;
            Ok(())
        }
    };
    (@heartbeat $config:ident, qos) => {
        if let Some(qos) = $config.qos.as_ref().filter(|q| q.heartbeat_interval_secs > 0) {
            if let Err(error) = $crate::qos::start_heartbeat(qos, &$config.tangle).await {
                tracing::warn!(%error, "QoS heartbeat unavailable");
            }
        }
    };
    (@heartbeat $config:ident, no_qos) => {};
}
