//! Replays the recorded Files API and Responses `file_id` lifecycles through the HTTP gateway.
//!
//! The recordings live in `agentic-server-core/tests/cassettes/text_files/responses/files-api-*.yaml`.
//! Each replay uploads the recorded multipart bytes, follows the recorded metadata, content, and delete
//! turns, and sends the recorded Responses turns with recorded file and response IDs remapped to the IDs
//! this gateway generated. A mock upstream returns the recorded answer, so these tests check the
//! gateway's file resolution, error mapping, and client-facing persistence rather than model output.

#[allow(dead_code)]
mod common;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use agentic_core::executor::ExecutionContext;
use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../agentic-server-core/tests/cassettes/text_files/responses"
);
const TEXT: &str = "files-api-text-gateway-Qwen-Qwen2.5-3B-Instruct.yaml";
const IMAGE: &str = "files-api-image-gateway-Qwen-Qwen2.5-VL-3B-Instruct.yaml";
const ERRORS: &str = "files-api-errors-gateway-Qwen-Qwen2.5-3B-Instruct.yaml";

fn load(name: &str) -> Value {
    let path = format!("{DIR}/{name}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    serde_yml::from_str(&text).unwrap_or_else(|error| panic!("parse {path}: {error}"))
}

fn turns(cassette: &Value) -> &[Value] {
    cassette["turns"].as_array().expect("cassette turns")
}

fn turn<'a>(cassette: &'a Value, filename: &str) -> &'a Value {
    turns(cassette)
        .iter()
        .find(|turn| turn["filename"] == filename)
        .unwrap_or_else(|| panic!("cassette turn {filename}"))
}

fn method(turn: &Value) -> &str {
    turn["request"]["method"].as_str().expect("request method")
}

fn path(turn: &Value) -> &str {
    turn["request"]["path"].as_str().expect("request path")
}

fn expected_status(turn: &Value) -> StatusCode {
    let status = u16::try_from(turn["response"]["status_code"].as_u64().expect("status code")).expect("u16 status");
    StatusCode::from_u16(status).expect("valid status code")
}

fn request_bytes(turn: &Value) -> Option<Vec<u8>> {
    turn["request"]["body_base64"]
        .as_str()
        .map(|encoded| STANDARD.decode(encoded).expect("base64 request body"))
}

fn response_bytes(turn: &Value) -> Vec<u8> {
    STANDARD
        .decode(turn["response"]["body_base64"].as_str().expect("base64 response body"))
        .expect("base64 response body")
}

fn recorded_content(cassette: &Value) -> Vec<u8> {
    response_bytes(turn(cassette, "t3"))
}

fn recorded_id(turn: &Value) -> &str {
    turn["response"]["body"]["id"].as_str().expect("recorded ID")
}

fn output_text(body: &Value) -> String {
    body["output"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect()
}

fn remap_ids(value: &mut Value, ids: &HashMap<String, String>) {
    match value {
        Value::String(text) => {
            if let Some(replacement) = ids.get(text.as_str()) {
                *text = replacement.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| remap_ids(item, ids)),
        Value::Object(fields) => fields.values_mut().for_each(|field| remap_ids(field, ids)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Replaces recorded file IDs anywhere an expected error message embeds them.
///
/// Uploads receive fresh IDs on replay, and the gateway echoes the client's ID in
/// `Files [<id>] were not found`, so error bodies cannot be compared verbatim.
fn remap_error_ids(value: &mut Value, ids: &HashMap<String, String>) {
    match value {
        Value::String(text) => {
            for (recorded, actual) in ids {
                *text = text.replace(recorded.as_str(), actual);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| remap_error_ids(item, ids)),
        Value::Object(fields) => fields.values_mut().for_each(|field| remap_error_ids(field, ids)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn remap_path(path: &str, ids: &HashMap<String, String>) -> String {
    path.split('/')
        .map(|segment| ids.get(segment).cloned().unwrap_or_else(|| segment.to_owned()))
        .collect::<Vec<_>>()
        .join("/")
}

fn assert_no_file_reference(request: &Value) {
    assert!(
        request.get("previous_response_id").is_none(),
        "the gateway rehydrates history itself: {request}"
    );
    let encoded = request.to_string();
    assert!(
        !encoded.contains("\"file_id\""),
        "the model must receive the resolved file: {encoded}"
    );
    assert!(
        !encoded.contains("\"file_data\""),
        "the model must receive the resolved file: {encoded}"
    );
}

async fn stored_parts(exec_ctx: &ExecutionContext) -> Vec<Value> {
    let pool = exec_ctx.storage_pool().expect("storage pool");
    let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM items")
        .fetch_all(pool)
        .await
        .expect("stored items");
    rows.iter()
        .flat_map(|row| {
            let item: Value = serde_json::from_str(row).expect("stored item JSON");
            item["content"].as_array().cloned().unwrap_or_default()
        })
        .collect()
}

/// The client-facing input context a stored response was generated from.
async fn client_input_items(gateway: &Gateway, response_id: &str) -> Vec<Value> {
    let url = format!("{}/v1/responses/{response_id}/input_items?order=asc", gateway.url);
    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .expect("input items request");
    assert_eq!(response.status(), StatusCode::OK, "input items status");
    let body: Value = response.json().await.expect("input items JSON");
    assert_eq!(body["object"], "list");
    body["data"].as_array().cloned().expect("input items data")
}

fn content_parts(items: &[Value]) -> Vec<&Value> {
    items
        .iter()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .collect()
}

async fn post_response(gateway: &Gateway, input: Value) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", gateway.url))
        .json(&json!({"model": "test-model", "input": input, "store": false, "stream": false}))
        .send()
        .await
        .expect("response request");
    let status = response.status();
    let body = response.json().await.expect("response JSON");
    (status, body)
}

fn error_message(body: &Value) -> &str {
    body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("error message: {body}"))
}

struct MockUpstream {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_upstream(responses: Vec<Value>) -> MockUpstream {
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let route_responses = Arc::clone(&responses);
    let route_requests = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| {
            let responses = Arc::clone(&route_responses);
            let requests = Arc::clone(&route_requests);
            async move {
                let request: Value = serde_json::from_slice(&body).expect("upstream request JSON");
                requests.lock().await.push(request);
                let response = responses.lock().await.pop_front();
                match response {
                    Some(response) => axum::Json(response).into_response(),
                    None => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": {"message": "mock upstream response queue exhausted"}})),
                    )
                        .into_response(),
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock upstream");
    let address = listener.local_addr().expect("mock upstream address");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve mock upstream") });
    MockUpstream {
        url: format!("http://{address}"),
        requests,
        task,
    }
}

struct Gateway {
    url: String,
    exec_ctx: Arc<ExecutionContext>,
    task: JoinHandle<()>,
    _db: tempfile::TempDir,
    _files: tempfile::TempDir,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway(upstream_url: &str) -> Gateway {
    start_gateway_with_file_search(upstream_url, true).await
}

async fn start_gateway_with_file_search(upstream_url: &str, file_search: bool) -> Gateway {
    let db = tempfile::tempdir().expect("temp database directory");
    let files = tempfile::tempdir().expect("temp files directory");
    let mut config = common::test_config(upstream_url);
    config.openai_api_key = None;
    config.db_url = Some(format!("sqlite://{}", db.path().join("gateway.db").display()));
    config.tools.file_search.files_storage_dir = Some(files.path().to_owned());
    let mut exec_ctx = ExecutionContext::from_config(&config).await.expect("execution context");
    if !file_search {
        exec_ctx = exec_ctx.without_file_search_service();
    }
    let exec_ctx = Arc::new(exec_ctx);
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec_ctx);
    let (url, task) = common::spawn_gateway(state).await;
    Gateway {
        url,
        exec_ctx,
        task,
        _db: db,
        _files: files,
    }
}

struct Replay {
    gateway: Gateway,
    upstream: MockUpstream,
    file_ids: HashMap<String, String>,
    response_ids: HashMap<String, String>,
}

async fn replay(cassette: &Value, expected_upstream_calls: usize) -> Replay {
    let responses = turns(cassette)
        .iter()
        .filter(|turn| path(turn) == "/v1/responses" && expected_status(turn) == StatusCode::OK)
        .map(|turn| turn["response"]["body"].clone())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), expected_upstream_calls);
    let upstream = mock_upstream(responses).await;
    let gateway = start_gateway(&upstream.url).await;
    let client = reqwest::Client::new();
    let mut file_ids = HashMap::new();
    let mut response_ids = HashMap::new();
    for turn in turns(cassette) {
        replay_turn(&client, &gateway.url, turn, &mut file_ids, &mut response_ids).await;
    }
    assert_eq!(
        upstream.requests.lock().await.len(),
        expected_upstream_calls,
        "rejected files must not reach inference"
    );
    Replay {
        gateway,
        upstream,
        file_ids,
        response_ids,
    }
}

async fn replay_turn(
    client: &reqwest::Client,
    gateway_url: &str,
    turn: &Value,
    file_ids: &mut HashMap<String, String>,
    response_ids: &mut HashMap<String, String>,
) {
    let method = method(turn);
    let raw_path = path(turn);
    let path = remap_path(raw_path, file_ids);
    let url = format!("{gateway_url}{path}");
    let mut request = match method {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "DELETE" => client.delete(url),
        other => panic!("unsupported method {other}"),
    };
    if let Some(body) = request_bytes(turn) {
        let content_type = turn["request"]["headers"]["content-type"]
            .as_str()
            .expect("request content type");
        request = request.header("content-type", content_type).body(body);
    } else if raw_path == "/v1/responses" {
        let mut body = turn["request"]["body"].clone();
        remap_ids(&mut body, response_ids);
        remap_ids(&mut body, file_ids);
        request = request.json(&body);
    }
    let response = request.send().await.expect("gateway request");
    let status = response.status();
    assert_eq!(
        status,
        expected_status(turn),
        "{} {raw_path} ({})",
        method,
        turn["filename"]
    );
    let expected = &turn["response"];
    if !status.is_success() {
        let actual: Value = response.json().await.expect("error JSON");
        let mut expected_body = expected["body"].clone();
        remap_error_ids(&mut expected_body, file_ids);
        assert_eq!(actual, expected_body, "{}", turn["filename"]);
        return;
    }
    if let Some(encoded) = expected["body_base64"].as_str() {
        let actual = response.bytes().await.expect("response bytes");
        assert_eq!(
            actual.as_ref(),
            STANDARD.decode(encoded).expect("expected bytes").as_slice(),
            "{}",
            turn["filename"]
        );
        return;
    }
    let actual: Value = response.json().await.expect("JSON response");
    match (method, raw_path) {
        ("POST", "/v1/files") => assert_upload(&actual, expected, file_ids),
        ("GET", path) if path.starts_with("/v1/files/") => assert_file_metadata(&actual, expected, file_ids),
        ("DELETE", path) if path.starts_with("/v1/files/") => assert_delete(&actual, expected, file_ids),
        ("POST", "/v1/responses") => assert_response(&actual, expected, response_ids),
        (method, path) => panic!("unhandled {method} {path}"),
    }
}

fn assert_upload(actual: &Value, expected: &Value, file_ids: &mut HashMap<String, String>) {
    let recorded_id = expected["body"]["id"].as_str().expect("recorded file ID");
    let actual_id = actual["id"].as_str().expect("uploaded file ID");
    assert!(actual_id.starts_with("file-"), "{actual}");
    assert_ne!(actual_id, recorded_id, "the replay must exercise file ID remapping");
    for field in ["bytes", "filename", "object", "purpose", "status"] {
        assert_eq!(actual[field], expected["body"][field], "upload {field}");
    }
    file_ids.insert(recorded_id.to_owned(), actual_id.to_owned());
}

fn assert_file_metadata(actual: &Value, expected: &Value, file_ids: &HashMap<String, String>) {
    let recorded_id = expected["body"]["id"].as_str().expect("recorded file ID");
    let actual_id = file_ids.get(recorded_id).expect("uploaded file ID");
    assert_eq!(actual["id"].as_str(), Some(actual_id.as_str()));
    for field in ["bytes", "filename", "object", "purpose", "status"] {
        assert_eq!(actual[field], expected["body"][field], "metadata {field}");
    }
}

fn assert_delete(actual: &Value, expected: &Value, file_ids: &HashMap<String, String>) {
    let recorded_id = expected["body"]["id"].as_str().expect("recorded file ID");
    let actual_id = file_ids.get(recorded_id).expect("uploaded file ID");
    assert_eq!(actual["deleted"], true);
    assert_eq!(actual["object"], "file.deleted");
    assert_eq!(actual["id"].as_str(), Some(actual_id.as_str()));
}

fn assert_response(actual: &Value, expected: &Value, response_ids: &mut HashMap<String, String>) {
    let recorded_id = expected["body"]["id"].as_str().expect("recorded response ID");
    let actual_id = actual["id"].as_str().expect("response ID");
    assert!(actual_id.starts_with("resp_"), "{actual}");
    assert_eq!(actual["status"], expected["body"]["status"]);
    assert_eq!(actual["model"], expected["body"]["model"]);
    assert_eq!(actual["store"], expected["body"]["store"]);
    assert_eq!(output_text(actual), output_text(&expected["body"]));
    match expected["body"]["previous_response_id"].as_str() {
        Some(recorded_parent) => {
            let parent = response_ids.get(recorded_parent).expect("parent response ID");
            assert_eq!(actual["previous_response_id"].as_str(), Some(parent.as_str()));
        }
        None => assert!(actual["previous_response_id"].is_null(), "{actual}"),
    }
    response_ids.insert(recorded_id.to_owned(), actual_id.to_owned());
}

#[tokio::test]
async fn files_api_text_reference_replays_through_the_gateway() {
    let cassette = load(TEXT);
    let Replay {
        gateway,
        upstream,
        file_ids,
        response_ids,
    } = replay(&cassette, 2).await;
    let notes = recorded_content(&cassette);
    let expected = format!(
        "File: notes.txt\n{}",
        std::str::from_utf8(&notes).expect("notes are UTF-8")
    );
    let requests = upstream.requests.lock().await;
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(
            request["input"][0]["content"][1],
            json!({"type": "input_text", "text": expected}),
            "the model receives the stored file as text"
        );
        assert_no_file_reference(request);
    }
    assert_eq!(
        requests[1]["input"][0]["content"][1],
        json!({"type": "input_text", "text": expected}),
        "a continuation rehydrates the stored file reference"
    );

    let first_file = file_ids.get(recorded_id(turn(&cassette, "t1"))).expect("first upload");
    let stored = stored_parts(&gateway.exec_ctx).await;
    let reference = stored
        .iter()
        .find(|part| part["type"] == "input_file" && part["file_id"].as_str() == Some(first_file.as_str()))
        .expect("stored history keeps the client's file_id");
    assert!(
        reference.get("file_data").is_none(),
        "stored history must not inline the file: {reference}"
    );
    assert!(
        !stored.iter().any(|part| part["text"]
            .as_str()
            .is_some_and(|text| text.contains("Deployment checklist"))),
        "model-facing file text must not be persisted"
    );

    let first_response = response_ids
        .get(recorded_id(turn(&cassette, "t4")))
        .expect("first response ID");
    let listed_items = client_input_items(&gateway, first_response).await;
    let listed = content_parts(&listed_items);
    let listed_reference = listed
        .iter()
        .find(|part| part["type"] == "input_file" && part["file_id"].as_str() == Some(first_file.as_str()))
        .expect("client-facing history keeps the file_id reference");
    assert!(
        listed_reference.get("file_data").is_none(),
        "client-facing history must not inline the file: {listed_reference}"
    );
}

#[tokio::test]
async fn files_api_image_reference_replays_through_the_gateway() {
    let cassette = load(IMAGE);
    let Replay {
        gateway,
        upstream,
        file_ids,
        response_ids,
    } = replay(&cassette, 2).await;
    let image = recorded_content(&cassette);
    let expected_url = format!("data:image/png;base64,{}", STANDARD.encode(&image));
    let requests = upstream.requests.lock().await;
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(
            request["input"][0]["content"][1],
            json!({"type": "input_image", "image_url": expected_url, "detail": "auto"}),
            "the model receives the stored image as a data URL"
        );
        assert_no_file_reference(request);
    }

    let first_file = file_ids.get(recorded_id(turn(&cassette, "t1"))).expect("first upload");
    let stored = stored_parts(&gateway.exec_ctx).await;
    let reference = stored
        .iter()
        .find(|part| part["type"] == "input_image" && part["file_id"].as_str() == Some(first_file.as_str()))
        .expect("stored history keeps the client's file_id");
    assert!(
        reference.get("image_url").is_none(),
        "stored history must not inline the image: {reference}"
    );

    let first_response = response_ids
        .get(recorded_id(turn(&cassette, "t4")))
        .expect("first response ID");
    let listed_items = client_input_items(&gateway, first_response).await;
    let listed = content_parts(&listed_items);
    let listed_reference = listed
        .iter()
        .find(|part| part["type"] == "input_image" && part["file_id"].as_str() == Some(first_file.as_str()))
        .expect("client-facing history keeps the file_id reference");
    assert!(
        listed_reference.get("image_url").is_none(),
        "client-facing history must not inline the image: {listed_reference}"
    );
}

#[tokio::test]
async fn files_api_upload_and_lookup_errors_replay_without_inference() {
    let cassette = load(ERRORS);
    let Replay {
        gateway,
        upstream,
        response_ids,
        ..
    } = replay(&cassette, 0).await;
    assert!(response_ids.is_empty(), "rejected turns must not create responses");
    assert!(upstream.requests.lock().await.is_empty());
    let pool = gateway.exec_ctx.storage_pool().expect("storage pool");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(pool)
        .await
        .expect("stored item count");
    assert_eq!(stored, 0, "rejected files must not create stored history");
}

#[tokio::test]
async fn file_reference_errors_keep_their_http_status_and_part_position() {
    let upstream = mock_upstream(Vec::new()).await;
    let gateway = start_gateway(&upstream.url).await;
    let cases = vec![
        (
            json!([{"role": "user", "content": [
                {"type": "input_image", "file_id": "file-1", "image_url": "data:image/png;base64,AA=="}
            ]}]),
            "Mutually exclusive parameters: 'input[0].content[0]'. Ensure you are only providing one of: \
             'file_id' or 'image_url'."
                .to_owned(),
        ),
        (
            json!([{"role": "user", "content": [
                {"type": "input_file", "file_id": "file-1", "file_data": "aGk=", "filename": "notes.txt"}
            ]}]),
            "Mutually exclusive parameters: 'input[0].content[0]'. Ensure you are only providing one of: \
             'file_id' or 'file_data'."
                .to_owned(),
        ),
        (
            json!([{"role": "user", "content": [
                {"type": "input_file", "file_id": "file-1", "file_url": "https://example.invalid/a.txt"}
            ]}]),
            "Mutually exclusive parameters: 'input[0].content[0]'. Ensure you are only providing one of: \
             'file_id' or 'file_url'."
                .to_owned(),
        ),
    ];
    for (input, message) in cases {
        let (actual_status, body) = post_response(&gateway, input.clone()).await;
        assert_eq!(actual_status, StatusCode::BAD_REQUEST, "{input}");
        assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
        assert_eq!(body["error"]["code"], "mutually_exclusive_parameters", "{body}");
        assert_eq!(body["error"]["param"], "input[0].content[0]", "{body}");
        assert_eq!(error_message(&body), message, "{input}");
    }

    let missing = format!("file-{}", uuid::Uuid::now_v7());
    let nested = json!([
        {"role": "user", "content": [{"type": "input_text", "text": "first"}]},
        {"role": "user", "content": [
            {"type": "input_text", "text": "a"},
            {"type": "input_text", "text": "b"},
            {"type": "input_file", "file_id": missing}
        ]}
    ]);
    let (status, body) = post_response(&gateway, nested).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert_eq!(body["error"]["code"], Value::Null, "{body}");
    assert_eq!(body["error"]["param"], Value::Null, "{body}");
    assert_eq!(error_message(&body), format!("Files [{missing}] were not found"));

    let no_store = start_gateway_with_file_search(&upstream.url, false).await;
    let nested = json!([
        {"role": "user", "content": [{"type": "input_text", "text": "first"}]},
        {"role": "user", "content": [
            {"type": "input_text", "text": "a"},
            {"type": "input_text", "text": "b"},
            {"type": "input_file", "file_id": "file-1"}
        ]}
    ]);
    let (status, body) = post_response(&no_store, nested).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        error_message(&body),
        "invalid request: input[1].content[2]: input_file with file_id requires the gateway file store; configure \
         file storage or send the content inline"
    );
    assert!(
        upstream.requests.lock().await.is_empty(),
        "rejected references must not reach inference"
    );
}
