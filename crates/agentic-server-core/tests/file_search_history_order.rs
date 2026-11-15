//! A `file_search` call's reasoning must stay before the call and its tool output on a
//! `previous_response_id` continuation. Hosted tool/web/MCP searches already keep that order
//! (#505/#519); file search was the remaining gap in #482 because its public item omits the
//! retrieved results, so the model-facing call/output pair is persisted at the public item's
//! position rather than ahead of the turn's output.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, DbPool, ResponseStore, create_pool_with_schema};
use agentic_core::tool::file_search::FileSearchService;
use agentic_core::types::file_search::{AttachFileRequest, CreateVectorStoreRequest, FileAttributes, FileSearchConfig};
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use either::Either;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

type Requests = Arc<Mutex<Vec<Value>>>;

async fn spawn_backend(responses: Vec<Value>) -> (String, Requests, tokio::task::JoinHandle<()>) {
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (route_responses, route_requests) = (Arc::clone(&responses), Arc::clone(&requests));
    let app = Router::new()
        .route(
            "/v1/responses",
            post(move |body: Bytes| {
                let (responses, requests) = (Arc::clone(&route_responses), Arc::clone(&route_requests));
                async move {
                    requests
                        .lock()
                        .await
                        .push(serde_json::from_slice(&body).expect("request JSON"));
                    let response = responses.lock().await.pop_front().expect("one response per request");
                    axum::response::Response::builder()
                        .status(200)
                        .header("Content-Type", "application/json")
                        .body(axum::body::Body::from(response.to_string()))
                        .expect("mock response")
                        .into_response()
                }
            }),
        )
        .route(
            "/v1/embeddings",
            post(|axum::Json(input): axum::Json<Value>| async move {
                let data: Vec<Value> = input["input"]
                    .as_array()
                    .expect("embedding input")
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        let text = text.as_str().unwrap_or("");
                        // Deterministic 2-D embeddings: the "coral" cluster vs. everything else.
                        let vector = if text.contains("coral") || text.contains("aquatic") {
                            vec![1.0, 0.0]
                        } else {
                            vec![0.0, 1.0]
                        };
                        json!({"index": index, "embedding": vector})
                    })
                    .collect();
                axum::Json(json!({"model":"fixture","data":data}))
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock backend");
    let address = listener.local_addr().expect("mock address");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    (format!("http://{address}"), requests, task)
}

static DATABASE: AtomicU64 = AtomicU64::new(0);

async fn context(base_url: &str) -> (Arc<ExecutionContext>, String, TempDir) {
    let url = format!(
        "sqlite:file:file_search_history_{}?mode=memory&cache=shared",
        DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let pool: Arc<DbPool> = create_pool_with_schema(Some(&url)).await.expect("storage pool");
    let files = tempfile::tempdir().expect("temp file storage dir");
    let config = FileSearchConfig {
        files_storage_dir: Some(files.path().to_owned()),
        embedding_base_url: Some(format!("{base_url}/v1")),
        embedding_model: Some("fixture".into()),
        embedding_api_key: None,
        ..FileSearchConfig::default()
    };
    let file_search = FileSearchService::new(Arc::clone(&pool), Arc::new(reqwest::Client::new()), config).unwrap();
    let store = file_search
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = file_search
        .upload_file("coral.txt", "text/plain", "assistants", b"coral reef habitat".to_vec())
        .await
        .unwrap();
    file_search
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                attributes: FileAttributes::default(),
                chunking_strategy: None,
            },
        )
        .await
        .unwrap();

    let context = Arc::new(
        ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
            Arc::new(reqwest::Client::new()),
            base_url.to_owned(),
        )
        .with_file_search_service(file_search),
    );
    (context, store.id, files)
}

fn upstream(id: &str, output: &Value) -> Value {
    json!({"id": id, "object": "response", "status": "completed", "model": "test", "created_at": 0, "output": output})
}

fn reasoning() -> Value {
    json!({"type": "reasoning", "id": "rs_1", "summary": [],
        "content": [{"type": "reasoning_text", "text": "search the store first"}]})
}

fn file_search_call() -> Value {
    json!({"type": "function_call", "id": "fc_1", "call_id": "call_1",
        "name": "file_search", "arguments": json!({"queries": ["coral"]}).to_string(), "status": "completed"})
}

fn message() -> Value {
    json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "done", "annotations": []}]})
}

fn model_input(request: &Value) -> &[Value] {
    request["input"].as_array().expect("model input array")
}

async fn run(body: Value, context: &Arc<ExecutionContext>) -> ResponsePayload {
    let request: RequestPayload = serde_json::from_value(body).expect("public request");
    match Box::pin(ExecuteRequest::new(request, Arc::clone(context)).run())
        .await
        .expect("blocking execution")
    {
        Either::Left(response) => response,
        Either::Right(_) => panic!("blocking request returned a stream"),
    }
}

#[tokio::test]
async fn file_search_continuation_keeps_reasoning_before_the_call() {
    let (base_url, requests, _task) = spawn_backend(vec![
        upstream("up_1", &json!([reasoning(), file_search_call()])),
        upstream("up_2", &json!([message()])),
        upstream("up_3", &json!([message()])),
    ])
    .await;
    let (context, store_id, _files) = context(&base_url).await;
    let tools = json!([{"type": "file_search", "vector_store_ids": [store_id]}]);

    let first = run(
        json!({
            "model": "test",
            "input": [{"type": "message", "role": "user", "content": "find it"}],
            "tools": tools,
            "store": true
        }),
        &context,
    )
    .await;
    assert_eq!(first.status, "completed", "{first:?}");

    // The public response exposes the file_search_call, but by default it carries no retrieved results.
    let public: Vec<Value> = first
        .output
        .iter()
        .map(|item| serde_json::to_value(item).expect("output item"))
        .collect();
    for item in &public {
        if item["type"] == "file_search_call" {
            assert!(
                item.get("results").is_none(),
                "public file_search_call must omit results: {item}"
            );
        }
    }
    let public_types: Vec<&str> = public.iter().filter_map(|item| item["type"].as_str()).collect();
    assert_eq!(public_types, ["reasoning", "file_search_call", "message"], "{public:?}");

    run(
        json!({"model":"test","input":[{"type":"message","role":"user","content":"more"}],"tools":tools,"store":true,
            "previous_response_id": first.id}),
        &context,
    )
    .await;

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 3, "two in-turn rounds plus one continuation round");

    // The live second round sees: user, reasoning, file_search call, its tool output.
    let live = model_input(&requests[1]);
    assert_eq!(live.len(), 4, "{live:?}");
    assert_eq!(live[1]["type"], "reasoning");
    assert_eq!(live[2]["type"], "function_call");
    assert_eq!(live[2]["name"], "file_search");
    assert_eq!(live[3]["type"], "function_call_output");
    assert_eq!(live[2]["call_id"], live[3]["call_id"]);

    // The continuation must show the model exactly the same prefix the live round did.
    let continuation = model_input(&requests[2]);
    assert_eq!(
        continuation[..live.len()],
        live[..],
        "continuation shares the live round's prefix"
    );
    assert_eq!(
        continuation[live.len()]["content"][0]["text"],
        "done",
        "the turn's assistant answer follows the search output: {continuation:?}"
    );
    assert_eq!(
        continuation[live.len() + 1]["content"],
        "more",
        "the new user message is last: {continuation:?}"
    );
}
