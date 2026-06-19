#[allow(dead_code)]
mod common;

use agentic_core::config::Config;
use common::{spawn_gateway, spawn_mock_llm, test_config, test_state};

fn test_config_no_key(llm_url: &str) -> Config {
    Config {
        openai_api_key: None,
        ..test_config(llm_url)
    }
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config_no_key(&format!("http://{dead_addr}")))).await;
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
async fn test_ready_returns_503_when_llm_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config_no_key(&format!("http://{dead_addr}")))).await;
    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();
    assert_eq!(resp.status(), 503);
}
