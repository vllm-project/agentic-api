use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use tokio::net::TcpListener;

use agentic_server::config::GatewayConfig;
use agentic_server::proxy::ProxyState;

fn test_config(llm_url: &str) -> GatewayConfig {
    GatewayConfig {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("env-llm-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
    }
}

fn test_config_no_key(llm_url: &str) -> GatewayConfig {
    GatewayConfig {
        openai_api_key: None,
        ..test_config(llm_url)
    }
}

async fn spawn_mock_llm() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/health", get(|| async { StatusCode::OK.into_response() }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

async fn spawn_gateway(config: GatewayConfig) -> (String, tokio::task::JoinHandle<()>) {
    let state = ProxyState::new(config).unwrap();
    let router = agentic_server::app::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn test_health_returns_200() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _h2) = spawn_gateway(config).await;

    let resp = reqwest::get(format!("{gw_url}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_health_returns_200_even_when_llm_down() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);

    let config = test_config_no_key(&format!("http://{dead_addr}"));
    let (gw_url, _h2) = spawn_gateway(config).await;

    let resp = reqwest::get(format!("{gw_url}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_ready_returns_200_when_llm_healthy() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _h2) = spawn_gateway(config).await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_ready_returns_503_when_llm_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);

    let config = test_config_no_key(&format!("http://{dead_addr}"));
    let (gw_url, _h2) = spawn_gateway(config).await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();
    assert_eq!(resp.status(), 503);
}
