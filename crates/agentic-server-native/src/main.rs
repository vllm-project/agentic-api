//! Composition root: the published server stays independent of native engine dependencies.

mod engine;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::config::Config;
use agentic_core::error::Error;
use agentic_server::model_capabilities::ServedModel;
use agentic_server::server::{DirectInferenceStatus, GatewayOptions, ServerError, run_direct, shutdown_signal};
use agentic_server::startup;
use agentic_vllm::{VllmBackend, model_config::ModelConfig};
use engine::Engine;
use futures::future::LocalBoxFuture;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const ENGINE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> Result<(), ServerError> {
    startup::main(Some(launch))
}

fn launch(
    config: Config,
    gateway: GatewayOptions<'_>,
    home: PathBuf,
    model: Option<String>,
) -> LocalBoxFuture<'_, Result<(), ServerError>> {
    Box::pin(async move {
        let recipe = ModelConfig::load(&home, model).map_err(|error| ServerError::Native(Box::new(error)))?;
        let timeout = Duration::try_from_secs_f64(config.llm_ready_timeout_s)
            .map_err(|_| Error::Config("startup timeout must be finite and non-negative".to_owned()))?;
        let engine_config = recipe
            .engine_config()
            .map_err(|error| ServerError::Native(Box::new(error)))?;
        let client_config = recipe.client_config(&engine_config, timeout);
        // Register before spawning and retain the same listener across every startup phase.
        let signal = shutdown_signal()?;
        tokio::pin!(signal);
        let mut engine = Engine::spawn(&engine_config)?;
        let result = async {
            let backend = tokio::select! {
                biased;
                () = &mut signal => return Ok(()),
                status = engine.wait_for_exit() => return Err(Error::LlmProcessExited { status: status?.to_string() }.into()),
                result = tokio::time::timeout(timeout, Box::pin(VllmBackend::connect_recipe(client_config, &recipe))) => {
                    result.map_err(|_| Error::Config("native model startup timed out".to_owned()))?
                        .map_err(|error| ServerError::Native(error))?
                }
            };
            let model = ServedModel::new(backend.model_name().to_owned(), backend.max_model_len());
            let backend = Arc::new(backend);
            let shutdown = CancellationToken::new();
            let result = async {
                let status = DirectInferenceStatus::new(model, backend.subscribe_readiness());
                let mut health = backend.subscribe_health();
                let gateway = run_direct(config, gateway, backend.clone(), status, shutdown.clone());
                tokio::pin!(gateway);
                let interruption = tokio::select! {
                    result = &mut gateway => return result,
                    () = &mut signal => Ok(()),
                    status = engine.wait_for_exit() => Err(Error::LlmProcessExited { status: status?.to_string() }.into()),
                    _ = health.wait_for(|healthy| !*healthy) => Err(ServerError::Native(Box::new(
                        io::Error::other("native engine connection became unhealthy"),
                    ))),
                };
                shutdown.cancel();
                let drained = gateway.await;
                if let Err(error) = &drained {
                    warn!(%error, "gateway drain failed");
                }
                interruption.and(drained)
            }.await;
            // All gateway owners have been dropped before the engine client is closed.
            if let Ok(backend) = Arc::try_unwrap(backend) {
                match tokio::time::timeout(ENGINE_SHUTDOWN_TIMEOUT, backend.shutdown()).await {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => warn!(%error, "native frontend shutdown incomplete"),
                    Err(_) => warn!("native frontend shutdown timed out"),
                }
            } else {
                warn!("native requests remained after the gateway drain deadline");
            }
            result
        }.await;
        let stopped = engine
            .shutdown(ENGINE_SHUTDOWN_TIMEOUT)
            .await
            .map_err(ServerError::from);
        if let Err(error) = &stopped {
            warn!(%error, "native engine shutdown failed");
        }
        result.and(stopped)
    })
}
