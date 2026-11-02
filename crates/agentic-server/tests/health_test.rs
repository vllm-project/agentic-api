mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::config::Config;
use agentic_core::executor::ExecutionContext;
use axum::Router;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect};
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

async fn spawn_upstream(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), upstream)
}

async fn spawn_gateway_for_upstream(
    app: Router,
    configure: impl FnOnce(&mut Config),
) -> (String, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let (upstream_url, upstream) = spawn_upstream(app).await;
    let mut config = test_config_no_key(&upstream_url);
    configure(&mut config);
    let (gateway_url, gateway) = spawn_gateway(test_state(&config)).await;
    (gateway_url, upstream, gateway)
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

#[tokio::test]
async fn test_ready_returns_503_when_llm_rejects_health_check() {
    let app = Router::new().route("/health", get(|| async { StatusCode::UNAUTHORIZED }));
    let (gw_url, upstream, gateway) = spawn_gateway_for_upstream(app, |_| {}).await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(resp.status(), 503);
    upstream.abort();
    gateway.abort();
}

#[tokio::test]
async fn test_ready_returns_503_when_llm_health_redirects_to_success() {
    let app = Router::new()
        .route("/health", get(|| async { Redirect::temporary("/login") }))
        .route("/login", get(|| async { StatusCode::OK }));
    let (gw_url, upstream, gateway) = spawn_gateway_for_upstream(app, |_| {}).await;

    let response = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    upstream.abort();
    gateway.abort();
}

#[tokio::test]
async fn test_ready_accepts_any_successful_llm_health_status() {
    let app = Router::new().route("/health", get(|| async { StatusCode::NO_CONTENT }));
    let (gw_url, upstream, gateway) = spawn_gateway_for_upstream(app, |_| {}).await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(resp.status(), 200);
    upstream.abort();
    gateway.abort();
}

#[tokio::test]
async fn test_ready_returns_503_when_llm_health_check_times_out() {
    let app = Router::new().route(
        "/health",
        get(|| async {
            std::future::pending::<()>().await;
            StatusCode::OK
        }),
    );
    let (gw_url, upstream, gateway) = spawn_gateway_for_upstream(app, |_| {}).await;

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reqwest::get(format!("{gw_url}/ready")),
    )
    .await
    .expect("readiness response must be bounded by the upstream timeout")
    .unwrap();

    assert_eq!(response.status(), 503);
    upstream.abort();
    gateway.abort();
}

#[tokio::test]
async fn test_ready_returns_cached_result_during_overlapping_dependency_probe() {
    for (cached, expected) in [
        (None, StatusCode::SERVICE_UNAVAILABLE),
        (Some(false), StatusCode::SERVICE_UNAVAILABLE),
        (Some(true), StatusCode::OK),
    ] {
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/health",
            get({
                let requests = Arc::clone(&requests);
                move || {
                    let requests = Arc::clone(&requests);
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    }
                }
            }),
        );
        let (llm_url, upstream) = spawn_upstream(app).await;
        let state = test_state(&test_config_no_key(&llm_url));
        let readiness_tracker = state.readiness_tracker.clone();
        if let Some(cached) = cached {
            assert!(readiness_tracker.try_start_probe().unwrap().finish(cached));
        }
        let active_probe = readiness_tracker
            .try_start_probe()
            .expect("reserve the only readiness probe permit");
        let (gw_url, gateway) = spawn_gateway(state).await;

        let response = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

        assert_eq!(response.status(), expected);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        drop(active_probe);
        upstream.abort();
        gateway.abort();
    }
}

#[tokio::test]
async fn test_ready_returns_503_when_database_pool_is_exhausted() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let mut config = test_config(&llm_url);
    config.db_url = Some("sqlite://?mode=memory".to_owned());
    let exec_ctx = Arc::new(
        ExecutionContext::from_config(&config)
            .await
            .expect("create execution context"),
    );
    let held_connection = exec_ctx
        .storage_pool()
        .expect("configured storage pool")
        .acquire()
        .await
        .expect("hold only database connection");
    let mut state = test_state(&config);
    state.exec_ctx = exec_ctx;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(resp.status(), 503);
    drop(held_connection);
}

#[tokio::test]
async fn test_ready_fails_fast_when_database_is_unready() {
    let app = Router::new().route(
        "/health",
        get(|| async {
            std::future::pending::<()>().await;
            StatusCode::OK
        }),
    );
    let (upstream_url, upstream) = spawn_upstream(app).await;
    let mut config = test_config_no_key(&upstream_url);
    config.db_url = Some("sqlite://?mode=memory".to_owned());
    let exec_ctx = Arc::new(
        ExecutionContext::from_config(&config)
            .await
            .expect("create execution context"),
    );
    let held_connection = exec_ctx
        .storage_pool()
        .expect("configured storage pool")
        .acquire()
        .await
        .expect("hold only database connection");
    let mut state = test_state(&config);
    state.exec_ctx = exec_ctx;
    let (gw_url, gateway) = spawn_gateway(state).await;

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reqwest::get(format!("{gw_url}/ready")),
    )
    .await
    .expect("database failure should return a bounded readiness response")
    .unwrap();

    assert_eq!(response.status(), 503);
    drop(held_connection);
    upstream.abort();
    gateway.abort();
}

#[tokio::test]
async fn test_ready_skips_upstream_when_configured() {
    let requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/health",
        get({
            let requests = Arc::clone(&requests);
            move || {
                let requests = Arc::clone(&requests);
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }
        }),
    );
    let (gw_url, upstream, gateway) = spawn_gateway_for_upstream(app, |config| {
        config.skip_llm_ready_check = true;
    })
    .await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    upstream.abort();
    gateway.abort();
}

#[tokio::test]
async fn test_ready_skip_still_requires_database() {
    let mut config = test_config("http://127.0.0.1:1");
    config.skip_llm_ready_check = true;
    config.db_url = Some("sqlite://?mode=memory".to_owned());
    let exec_ctx = Arc::new(
        ExecutionContext::from_config(&config)
            .await
            .expect("create execution context"),
    );
    let held_connection = exec_ctx
        .storage_pool()
        .expect("configured storage pool")
        .acquire()
        .await
        .expect("hold only database connection");
    let mut state = test_state(&config);
    state.exec_ctx = exec_ctx;
    let (gw_url, gateway) = spawn_gateway(state).await;

    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(resp.status(), 503);
    drop(held_connection);
    gateway.abort();
}

#[tokio::test]
async fn test_ready_authenticates_upstream_health_check() {
    let app = Router::new().route(
        "/health",
        get(|headers: HeaderMap| async move {
            if headers.get("authorization").and_then(|value| value.to_str().ok()) == Some("Bearer test-key") {
                StatusCode::OK.into_response()
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }),
    );
    let (gw_url, upstream, gateway) = spawn_gateway_for_upstream(app, |config| {
        config.openai_api_key = Some("test-key".to_owned());
    })
    .await;
    let resp = reqwest::get(format!("{gw_url}/ready")).await.unwrap();

    assert_eq!(resp.status(), 200);
    upstream.abort();
    gateway.abort();
}
