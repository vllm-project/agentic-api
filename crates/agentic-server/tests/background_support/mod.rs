//! Shared fixtures for the background-mode HTTP tests: a scripted model server,
//! a gateway-executed tool that can be held open, and a gateway backed by a
//! file `SQLite` database so several execution contexts can share one store.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::routing::post;
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinHandle;

use agentic_core::config::Config;
use agentic_core::executor::ExecutionContext;
use agentic_core::tool::{GatewayExecutor, ToolError, ToolHandler, ToolOutput, ToolType};
use agentic_core::types::io::FunctionTool;
use agentic_core::types::tools::WebSearchToolParam;

use crate::common::{spawn_gateway, test_config, test_state};

/// Upper bound for every await in these tests.
pub const LIMIT: Duration = Duration::from_secs(10);

/// Await `future` or fail the test after [`LIMIT`].
pub async fn bounded<F: Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(LIMIT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// The database for one test: a file `SQLite` database removed on drop or,
/// when `AGENTIC_BACKGROUND_TEST_POSTGRES_URL` names a PostgreSQL server with
/// CREATE DATABASE privileges, a new database on that server.
pub struct TestDb {
    url: String,
    sqlite: Option<PathBuf>,
}

impl TestDb {
    pub async fn new() -> Self {
        let base_url = match std::env::var("AGENTIC_BACKGROUND_TEST_POSTGRES_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => {
                let path = std::env::temp_dir().join(format!("agentic_background_test_{}.db", uuid::Uuid::now_v7()));
                return Self {
                    url: format!("sqlite://{}", path.display()),
                    sqlite: Some(path),
                };
            }
            Err(error) => panic!("invalid PostgreSQL test configuration: {error}"),
        };
        let mut url = url::Url::parse(&base_url).expect("valid PostgreSQL fixture URL");
        assert!(matches!(url.scheme(), "postgres" | "postgresql"));
        let admin = agentic_core::storage::create_pool(Some(&base_url))
            .await
            .expect("PostgreSQL admin pool");
        // A UUID-only name: each case owns a new database and never drops the caller's.
        let database = format!("background_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(admin.as_ref())
            .await
            .expect("create the test database");
        admin.close().await;
        url.set_path(&database);
        Self {
            url: url.to_string(),
            sqlite: None,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if let Some(path) = &self.sqlite {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("db-shm"));
            let _ = std::fs::remove_file(path.with_extension("db-wal"));
        }
    }
}

/// One scripted model turn.
#[derive(Clone, Copy, Debug)]
pub enum Turn {
    /// Answer with one assistant message.
    Answer(&'static str),
    /// Ask the gateway to run its `web_search` tool.
    Search,
    /// Hold the request open until [`Model::release`], then answer.
    Hold(&'static str),
}

/// Counts a held request that ends without being released: the gateway dropped it.
struct DropProbe {
    dropped: Arc<AtomicUsize>,
    armed: bool,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        if self.armed {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// A model server that plays [`Turn`]s in request order, then answers `"done"`.
pub struct Model {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    started: mpsc::Receiver<usize>,
    release: Arc<Semaphore>,
    dropped: Arc<AtomicUsize>,
    _server: JoinHandle<()>,
}

fn answer(text: &str) -> Value {
    json!({
        "id": "resp_upstream", "object": "response", "created_at": 1, "model": "test-model",
        "status": "completed",
        "output": [{"type": "message", "id": format!("msg_{text}"), "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
}

fn search_call() -> Value {
    json!({
        "id": "resp_upstream", "object": "response", "created_at": 1, "model": "test-model",
        "status": "completed",
        "output": [{"id": "fc_search", "type": "function_call", "call_id": "call_search", "name": "web_search",
            "arguments": "{\"query\":\"background\"}", "status": "completed"}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
}

impl Model {
    pub async fn start(script: Vec<Turn>) -> Self {
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started) = mpsc::channel(16);
        let release = Arc::new(Semaphore::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/responses",
            post({
                let (script, requests, release, dropped) = (
                    Arc::clone(&script),
                    Arc::clone(&requests),
                    Arc::clone(&release),
                    Arc::clone(&dropped),
                );
                move |body: Bytes| {
                    let (script, requests, release, dropped, started_tx) = (
                        Arc::clone(&script),
                        Arc::clone(&requests),
                        Arc::clone(&release),
                        Arc::clone(&dropped),
                        started_tx.clone(),
                    );
                    async move {
                        let index = {
                            let mut requests = requests.lock().await;
                            requests.push(serde_json::from_slice(&body).expect("model request is JSON"));
                            requests.len() - 1
                        };
                        let turn = script.lock().await.pop_front().unwrap_or(Turn::Answer("done"));
                        let _ = started_tx.try_send(index);
                        axum::Json(match turn {
                            Turn::Answer(text) => answer(text),
                            Turn::Search => search_call(),
                            Turn::Hold(text) => {
                                let mut probe = DropProbe { dropped, armed: true };
                                release.acquire().await.expect("release semaphore open").forget();
                                probe.armed = false;
                                answer(text)
                            }
                        })
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind model");
        let url = format!("http://{}", listener.local_addr().expect("model address"));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve model") });
        Self {
            url,
            requests,
            started,
            release,
            dropped,
            _server: server,
        }
    }

    /// Wait until the model has received request number `index` (zero-based).
    pub async fn wait_started(&mut self, index: usize) {
        loop {
            let started = bounded("a model request", self.started.recv()).await;
            if started.expect("model server running") >= index {
                return;
            }
        }
    }

    /// Let one held request answer.
    pub fn release(&self) {
        self.release.add_permits(1);
    }

    pub async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    /// Wait until `count` held requests were dropped by the gateway.
    pub async fn wait_dropped(&self, count: usize) {
        bounded("the gateway to drop a held model request", async {
            while self.dropped.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

/// An MCP server that lists one `forecast` tool: enough for request preparation to discover it.
pub async fn mcp_server() -> String {
    let app = Router::new().route(
        "/mcp",
        post(|axum::Json(request): axum::Json<Value>| async move {
            let id = request["id"].clone();
            if id.is_null() {
                // A notification such as `notifications/initialized`.
                return axum::response::IntoResponse::into_response(StatusCode::ACCEPTED);
            }
            let reply = match request["method"].as_str() {
                Some("initialize") => json!({"result": {
                    "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub", "version": "0"}
                }}),
                Some("tools/list") => json!({"result": {"tools": [{
                    "name": "forecast", "description": "Forecast a city.",
                    "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}}
                }]}}),
                _ => json!({"error": {"code": -32601, "message": "method not found"}}),
            };
            let mut body = json!({"jsonrpc": "2.0", "id": id});
            body.as_object_mut()
                .expect("reply object")
                .extend(reply.as_object().expect("reply object").clone());
            axum::response::IntoResponse::into_response(axum::Json(body))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind MCP server");
    let address = listener.local_addr().expect("MCP address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve MCP") });
    format!("http://{address}/mcp")
}

/// A gateway `web_search` tool that stays open until the gateway drops it.
#[derive(Clone)]
pub struct HeldSearch {
    started: Arc<Semaphore>,
    dropped: Arc<AtomicUsize>,
}

impl HeldSearch {
    pub fn new() -> Self {
        Self {
            started: Arc::new(Semaphore::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn wait_started(&self) {
        bounded("the gateway tool call", self.started.acquire())
            .await
            .expect("tool semaphore open")
            .forget();
    }

    pub async fn wait_dropped(&self) {
        bounded("the gateway to drop its tool call", async {
            while self.dropped.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }
}

impl ToolHandler for HeldSearch {
    type ToolParams = WebSearchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }

    fn validate(&self, _params: &WebSearchToolParam) -> Result<(), ToolError> {
        Ok(())
    }

    fn normalize(&self, _params: &WebSearchToolParam) -> Vec<FunctionTool> {
        vec![FunctionTool {
            type_: "function".to_owned(),
            name: "web_search".to_owned(),
            description: Some("Search the web.".to_owned()),
            parameters: Some(json!({
                "type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]
            })),
            strict: Some(false),
        }]
    }
}

impl GatewayExecutor for HeldSearch {
    type ExecutionParams = WebSearchToolParam;

    fn execute(
        &self,
        _call_id: &str,
        _tool_name: &str,
        _arguments: &str,
        _params: &WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let probe = DropProbe {
            dropped: Arc::clone(&self.dropped),
            armed: true,
        };
        self.started.add_permits(1);
        Box::pin(async move {
            let _probe = probe;
            std::future::pending::<Result<ToolOutput, ToolError>>().await
        })
    }
}

/// A gateway serving one execution context over a shared file database.
pub struct Gateway {
    pub url: String,
    pub exec: Arc<ExecutionContext>,
    pub client: reqwest::Client,
    _server: JoinHandle<()>,
}

impl Gateway {
    /// Configuration for a gateway in front of `model_url`, storing in `db`.
    pub fn config(model_url: &str, db: &TestDb) -> Config {
        let mut config = test_config(model_url);
        config.db_url = Some(db.url());
        config
    }

    pub async fn start(config: &Config) -> Self {
        let exec = ExecutionContext::from_config(config).await.expect("execution context");
        Self::serve(config, exec).await
    }

    pub async fn start_with_search(config: &Config, search: &HeldSearch) -> Self {
        let exec = ExecutionContext::from_config(config).await.expect("execution context");
        Self::serve(config, exec.with_gateway_executor(Arc::new(search.clone()))).await
    }

    async fn serve(config: &Config, exec: ExecutionContext) -> Self {
        let exec = Arc::new(exec);
        let mut state = test_state(config);
        state.exec_ctx = Arc::clone(&exec);
        let (url, server) = spawn_gateway(state).await;
        Self {
            url,
            exec,
            client: reqwest::Client::new(),
            _server: server,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = bounded("a gateway response", request.bearer_auth("test-key").send())
            .await
            .expect("gateway reachable");
        let status = response.status();
        let body = bounded("a gateway response body", response.json::<Value>())
            .await
            .expect("JSON body");
        (status, body)
    }

    pub async fn create(&self, request: Value) -> (StatusCode, Value) {
        self.send(self.client.post(format!("{}/v1/responses", self.url)).json(&request))
            .await
    }

    /// Create a background response and return its queued snapshot.
    pub async fn create_background(&self, extra: Value) -> Value {
        let mut request = json!({"model": "test-model", "input": "hi", "background": true});
        request
            .as_object_mut()
            .expect("request object")
            .extend(extra.as_object().expect("extra object").clone());
        let (status, body) = self.create(request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    pub async fn get(&self, id: &str) -> (StatusCode, Value) {
        self.send(self.client.get(format!("{}/v1/responses/{id}", self.url)))
            .await
    }

    pub async fn get_with_query(&self, id: &str, query: &str) -> (StatusCode, Value) {
        self.send(self.client.get(format!("{}/v1/responses/{id}?{query}", self.url)))
            .await
    }

    pub async fn cancel(&self, id: &str) -> (StatusCode, Value) {
        self.send(self.client.post(format!("{}/v1/responses/{id}/cancel", self.url)))
            .await
    }

    pub async fn delete(&self, id: &str) -> (StatusCode, Value) {
        self.send(self.client.delete(format!("{}/v1/responses/{id}", self.url)))
            .await
    }

    pub async fn create_conversation(&self) -> String {
        let (status, body) = self
            .send(
                self.client
                    .post(format!("{}/v1/conversations", self.url))
                    .json(&json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["id"].as_str().expect("conversation id").to_owned()
    }

    /// The text of each item in a conversation, oldest first.
    pub async fn conversation_texts(&self, conversation: &str) -> Vec<String> {
        let (status, body) = self
            .send(
                self.client
                    .get(format!("{}/v1/conversations/{conversation}/items?order=asc", self.url)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"]
            .as_array()
            .expect("item list")
            .iter()
            .map(|item| item["content"][0]["text"].as_str().expect("text item").to_owned())
            .collect()
    }

    pub async fn input_items(&self, id: &str) -> (StatusCode, Value) {
        self.send(self.client.get(format!("{}/v1/responses/{id}/input_items", self.url)))
            .await
    }

    /// Poll `GET /v1/responses/{id}` until its status is `status`.
    pub async fn wait_for_status(&self, id: &str, status: &str) -> Value {
        bounded(&format!("{id} to become {status}"), async {
            loop {
                let (code, body) = self.get(id).await;
                assert_eq!(code, StatusCode::OK, "{body}");
                if body["status"] == status {
                    return body;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }
}

pub fn id(response: &Value) -> &str {
    response["id"].as_str().expect("response id")
}
