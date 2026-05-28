use agentic_server::app::build_router;
use agentic_server::config::GatewayConfig;
use agentic_server::error::Error;
use agentic_server::proxy::ProxyState;
use agentic_server::readiness::wait_llm_ready;
use tokio::net::TcpListener;
use tracing::info;

async fn serve_gateway(config: GatewayConfig, host: &str, port: u16) -> Result<(), Error> {
    let addr = format!("{host}:{port}");
    let state = ProxyState::new(config)?;
    let router = build_router(state);
    let listener = TcpListener::bind(&addr).await?;
    info!("gateway listening on {addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Start the gateway after the LLM becomes ready.
///
/// # Errors
///
/// Returns an error if LLM readiness polling fails or the server cannot bind.
pub async fn run(config: GatewayConfig, host: &str, port: u16) -> Result<(), Error> {
    wait_llm_ready(&config).await?;
    info!("LLM ready: {}", config.llm_api_base);
    serve_gateway(config, host, port).await
}

/// Spawn vLLM as a subprocess and run the gateway in the foreground.
///
/// # Errors
///
/// Returns an error if vLLM fails to start or the gateway errors.
pub async fn run_with_llm(config: GatewayConfig, host: &str, port: u16, llm_args: Vec<String>) -> Result<(), Error> {
    let mut cmd = tokio::process::Command::new("python");
    cmd.arg("-m").arg("vllm.entrypoints.openai.api_server");
    cmd.args(&llm_args);

    let mut child = cmd.spawn()?;
    info!("spawned vLLM subprocess (pid {})", child.id().unwrap_or(0));

    let readiness_result = tokio::select! {
        ready = wait_llm_ready(&config) => ready,
        status = child.wait() => {
            let status = status?;
            Err(Error::LlmProcessExited {
                status: status.to_string(),
            })
        }
    };

    match readiness_result {
        Ok(()) => info!("LLM ready: {}", config.llm_api_base),
        Err(err) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(err);
        }
    }

    let result = tokio::select! {
        gateway = serve_gateway(config, host, port) => gateway,
        status = child.wait() => {
            let status = status?;
            Err(Error::LlmProcessExited {
                status: status.to_string(),
            })
        }
    };

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
