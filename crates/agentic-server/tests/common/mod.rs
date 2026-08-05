use std::path::PathBuf;
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
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_server::app::{AppState, ServerConfig, WebSocketTracker, build_router};

pub fn test_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("test-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
        skip_llm_ready_check: false,
        db_url: None,
        postgres: agentic_core::config::PostgresConfig::default(),
        sqlite: agentic_core::config::SqliteConfig::default(),
    }
}

pub fn test_state(config: &Config) -> AppState {
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
    );
    let exec_ctx = Arc::new(exec_ctx);
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");
    AppState {
        proxy_state,
        exec_ctx,
        shutdown_token: CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base.clone(),
        openai_api_key: config.openai_api_key.clone(),
    }
}

#[allow(dead_code)]
struct TestDb {
    path: PathBuf,
}

#[allow(dead_code)]
impl TestDb {
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!("agentic_http_test_{}.db", uuid::Uuid::now_v7())),
        }
    }

    fn url(&self) -> String {
        format!("sqlite://{}", self.path.display())
    }
}

#[allow(dead_code)]
impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
        let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
    }
}

#[allow(dead_code)]
pub struct StorageBackedState {
    pub state: AppState,
    _db: TestDb,
}

#[allow(dead_code)]
pub async fn storage_backed_state(llm_url: &str) -> StorageBackedState {
    let db = TestDb::new();
    let pool = create_pool_with_schema(Some(&db.url()))
        .await
        .expect("create test database");
    let config = test_config(llm_url);
    let exec_ctx = Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
        ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
    ));
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");
    let state = AppState {
        proxy_state,
        exec_ctx,
        shutdown_token: CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base,
        openai_api_key: config.openai_api_key,
    };
    StorageBackedState { state, _db: db }
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
