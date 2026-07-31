mod common;

use agentic_core::config::Config;
use axum::Router;
use axum::http::HeaderMap;
use axum::routing::get;
use common::{spawn_gateway, spawn_mock_llm, test_config, test_state};
use http::StatusCode;
use tokio::net::TcpListener;

fn test_config_no_key(llm_url: &str) -> Config {
    Config {
        openai_api_key: None,
        ..test_config(llm_url)
    }
}

async fn spawn_health_mock(status: StatusCode) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/health", get(move || async move { status }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

async fn spawn_authenticated_health_mock() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/health",
        get(|headers: HeaderMap| async move {
            match headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
            {
                Some("Bearer test-key") => StatusCode::OK,
                _ => StatusCode::UNAUTHORIZED,
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn test_health_returns_200() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config(&llm_url))).await;
    let resp = reqwest::get(format!("{gw_url}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_health_returns_200_even_when_llm_down() {
    let (llm_url, _h1) = spawn_health_mock(StatusCode::SERVICE_UNAVAILABLE).await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config_no_key(&llm_url))).await;
    let resp = reqwest::get(format!("{gw_url}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_ready_returns_200_when_llm_healthy() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config(&llm_url))).await;
    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_ready_returns_503_when_llm_unhealthy() {
    let (llm_url, _h1) = spawn_health_mock(StatusCode::SERVICE_UNAVAILABLE).await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config_no_key(&llm_url))).await;
    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn test_ready_forwards_configured_api_key() {
    let (llm_url, _h1) = spawn_authenticated_health_mock().await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config(&llm_url))).await;
    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();
    assert_eq!(resp.status(), 200);
}
