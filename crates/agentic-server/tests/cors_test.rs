use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use tokio::net::TcpListener;

use agentic_core::config::Config;
use agentic_core::proxy::ProxyState;

fn test_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: None,
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
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

async fn spawn_gateway(config: Config) -> (String, tokio::task::JoinHandle<()>) {
    let state = ProxyState::new(config).unwrap();
    let server_config = agentic_server::app::ServerConfig::from_env();
    let router = agentic_server::app::build_router(state, &server_config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn test_cors_preflight_returns_200() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{gw_url}/v1/responses"))
        .header("Origin", "http://example.com")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "Content-Type,Authorization")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("access-control-allow-origin"));
    assert!(resp.headers().contains_key("access-control-allow-methods"));
    assert!(resp.headers().contains_key("access-control-allow-headers"));
}

#[tokio::test]
async fn test_cors_headers_on_regular_request() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .header("Origin", "http://example.com")
        .header("Content-Type", "application/json")
        .body(r#"{"model":"test","input":"hi"}"#)
        .send()
        .await
        .unwrap();

    assert!(resp.headers().contains_key("access-control-allow-origin"));
}
