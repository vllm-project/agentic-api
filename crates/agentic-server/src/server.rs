use std::sync::Arc;

use agentic_core::config::Config;
use agentic_core::error::Error;
use agentic_core::executor::ExecutionContext;
use agentic_core::proxy::ProxyState;
use agentic_core::readiness::wait_llm_ready;
use agentic_server::app::{AppState, ServerConfig, build_router};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

async fn build_state(config: &Config, shutdown_token: CancellationToken) -> Result<AppState, Error> {
    let proxy_state = ProxyState::new(config.clone())?;
    let exec_ctx = Arc::new(ExecutionContext::from_config(config).await?);

    Ok(AppState {
        proxy_state,
        exec_ctx,
        shutdown_token,
        llm_api_base: config.llm_api_base.clone(),
        openai_api_key: config.openai_api_key.clone(),
    })
}

async fn serve_gateway(state: AppState, host: &str, port: u16) -> Result<(), Error> {
    let addr = format!("{host}:{port}");
    let server_config = ServerConfig::from_env();
    let shutdown_token = state.shutdown_token.clone();
    let router = build_router(state, &server_config);
    let listener = TcpListener::bind(&addr).await?;
    info!("gateway listening on {addr}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
        })
        .await?;
    Ok(())
}

async fn serve_gateway_until_signal(state: AppState, host: &str, port: u16) -> Result<(), Error> {
    let shutdown_token = state.shutdown_token.clone();
    let gateway = serve_gateway(state, host, port);
    tokio::pin!(gateway);

    tokio::select! {
        result = &mut gateway => result,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            info!("shutdown signal received");
            shutdown_token.cancel();
            gateway.await
        }
    }
}

async fn wait_until_llm_ready(config: &Config) -> Result<(), Error> {
    if config.skip_llm_ready_check {
        info!("skipping LLM readiness check: {}", config.llm_api_base);
        return Ok(());
    }

    wait_llm_ready(config).await?;
    info!("LLM ready: {}", config.llm_api_base);
    Ok(())
}

/// Start the gateway after the LLM becomes ready.
///
/// # Errors
///
/// Returns an error if DB initialisation, LLM readiness polling, or the
/// server binding fails.
pub async fn run(config: Config, host: &str, port: u16) -> Result<(), Error> {
    wait_until_llm_ready(&config).await?;
    let state = build_state(&config, CancellationToken::new()).await?;
    serve_gateway_until_signal(state, host, port).await
}

/// Spawn vLLM as a subprocess and run the gateway in the foreground.
///
/// # Errors
///
/// Returns an error if vLLM fails to start, DB init fails, or the gateway
/// errors.
pub async fn run_with_llm(config: Config, host: &str, port: u16, llm_args: Vec<String>) -> Result<(), Error> {
    let mut cmd = tokio::process::Command::new("python");
    cmd.arg("-m").arg("vllm.entrypoints.openai.api_server");
    cmd.args(&llm_args);

    let mut child = cmd.spawn()?;
    info!("spawned vLLM subprocess (pid {})", child.id().unwrap_or(0));

    let readiness_result = if config.skip_llm_ready_check {
        info!("skipping LLM readiness check: {}", config.llm_api_base);
        Ok(false)
    } else {
        tokio::select! {
            ready = wait_llm_ready(&config) => ready.map(|()| true),
            status = child.wait() => {
                let status = status?;
                Err(Error::LlmProcessExited { status: status.to_string() })
            }
        }
    };

    match readiness_result {
        Ok(true) => info!("LLM ready: {}", config.llm_api_base),
        Ok(false) => {}
        Err(err) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(err);
        }
    }

    let shutdown_token = CancellationToken::new();
    let state = match build_state(&config, shutdown_token.clone()).await {
        Ok(s) => s,
        Err(err) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(err);
        }
    };

    let gateway = serve_gateway(state, host, port);
    tokio::pin!(gateway);

    let result = tokio::select! {
        gateway = &mut gateway => gateway,
        status = child.wait() => {
            shutdown_token.cancel();
            let status = status?;
            Err(Error::LlmProcessExited { status: status.to_string() })
        },
        signal = tokio::signal::ctrl_c() => {
            signal?;
            info!("shutdown signal received");
            shutdown_token.cancel();
            gateway.await
        }
    };

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
