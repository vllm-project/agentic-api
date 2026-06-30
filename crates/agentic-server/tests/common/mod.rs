use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use agentic_core::config::Config;
use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::proxy::ProxyState;
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_server::app::{AppState, ServerConfig, build_router};

pub fn test_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("test-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
        db_url: None,
    }
}

pub fn test_state(config: &Config) -> AppState {
    let exec_ctx = Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
        config.openai_api_key.clone(),
    ));
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");
    AppState {
        proxy_state,
        exec_ctx,
        shutdown_token: CancellationToken::new(),
        llm_api_base: config.llm_api_base.clone(),
    }
}

/// Spawn a minimal mock LLM that responds to `GET /health` with 200.
pub async fn spawn_mock_llm() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/health", get(|| async { StatusCode::OK.into_response() }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

/// Spawn the gateway router bound to a random port.
pub async fn spawn_gateway(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let router = build_router(state, &ServerConfig::from_env());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), handle)
}
