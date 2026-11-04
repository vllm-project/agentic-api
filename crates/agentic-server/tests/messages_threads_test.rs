//! Local HTTP contract for Anthropic message threads against a stateless backend.
#[allow(dead_code)]
mod common;
#[path = "common/messages_database.rs"]
mod messages_database;

use std::fmt::Write as _;
use std::sync::Arc;

use agentic_core::types::messages::thread::{CLAUDE_CODE_BETA, MESSAGE_THREADS_BETA};
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

struct Server(tokio::task::JoinHandle<()>, Option<tempfile::TempDir>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type Requests = Arc<Mutex<Vec<Value>>>;

async fn infer(State(requests): State<Requests>, headers: HeaderMap, Json(request): Json<Value>) -> Response {
    assert!(request.get("thread").is_none());
    assert!(
        headers
            .get("anthropic-beta")
            .is_none_or(|value| !value.to_str().unwrap().contains(MESSAGE_THREADS_BETA))
    );
    requests.lock().await.push(request.clone());
    let last_content = &request["messages"].as_array().unwrap().last().unwrap()["content"];
    let content = if request["metadata"]["gateway_tool"] == true && last_content[0]["type"] != "tool_result" {
        json!([{"type":"tool_use","id":"gateway-call","name":"web_search","input":{"query":"local proof"}}])
    } else if request["metadata"]["client_tool"] == true {
        json!([{"type":"tool_use","id":"client-call","name":"echo","input":{"text":"value"}}])
    } else {
        json!([{"type":"text","text":"remembered 雪"}])
    };
    let stop = if content[0]["type"] == "tool_use" {
        "tool_use"
    } else {
        "end_turn"
    };
    let mut message = json!({"type":"message","id":"upstream-id","role":"assistant","model":"local-model",
        "content":content,"stop_reason":stop,"usage":{"input_tokens":4,"output_tokens":2}});
    if request["metadata"]["fallback"] == true {
        message["usage"]["iterations"] = json!([{"type":"fallback_message","model":"fallback-model"}]);
    }
    if request["stream"] == true {
        let mut start = message.clone();
        start["content"] = json!([]);
        start["stop_reason"] = Value::Null;
        let mut events = vec![json!({"type":"message_start","message":start})];
        for (index, block) in content.as_array().unwrap().iter().enumerate() {
            let mut skeleton = block.clone();
            let delta = if block["type"] == "tool_use" {
                skeleton["input"] = json!({});
                json!({"type":"input_json_delta","partial_json":block["input"].to_string()})
            } else {
                skeleton["text"] = json!("");
                json!({"type":"text_delta","text":block["text"]})
            };
            events.extend([
                json!({"type":"content_block_start","index":index,"content_block":skeleton}),
                json!({"type":"content_block_delta","index":index,"delta":delta}),
                json!({"type":"content_block_stop","index":index}),
            ]);
        }
        events.push(json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":message["usage"]}));
        if request["metadata"]["truncate"] != true && request["metadata"]["hang"] != true {
            events.push(json!({"type":"message_stop"}));
        }
        let mut encoded = String::new();
        for event in events {
            write!(encoded, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
        }
        let body = if request["metadata"]["hang"] == true {
            let prefix = futures::stream::once(std::future::ready(Ok::<_, std::convert::Infallible>(encoded)));
            Body::from_stream(prefix.chain(futures::stream::pending()))
        } else {
            Body::from(encoded)
        };
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
    } else {
        Json(message).into_response()
    }
}

async fn setup() -> (String, Requests, Server, Server) {
    let (url, requests, upstream, gateway, _) = setup_database().await;
    (url, requests, upstream, gateway)
}

async fn setup_database() -> (String, Requests, Server, Server, String) {
    let requests = Requests::default();
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/search", get(|| async { Json(json!({"results":{"web":[{"url":"https://example.com", "title":"local", "description":"local proof"}]}})) }))
        .with_state(Arc::clone(&requests));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = Server(
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        None,
    );
    let mut state = common::test_state(&common::test_config(&upstream));
    let (database_url, directory) = messages_database::database().await;
    let pool = agentic_core::storage::create_pool_with_schema(Some(&database_url))
        .await
        .unwrap();
    Arc::make_mut(&mut state.exec_ctx).messages_threads =
        agentic_core::executor::messages_threads::MessagesThreadStore::new(pool);
    let client = Arc::clone(&state.exec_ctx.client);
    Arc::make_mut(&mut state.exec_ctx).gateway_executors.insert(Arc::new(
        agentic_core::tool::WebSearchHandler::with_api_key(client, "test-key".to_owned(), &upstream),
    ));
    let (url, gateway) = common::spawn_gateway(state).await;
    (url, requests, server, Server(gateway, directory), database_url)
}

fn create(stream: bool) -> Value {
    json!({"model":"local-model","max_tokens":32,"stream":stream,"thread":{"type":"create"},
        "system":[{"type":"text","text":"Remember the image.","cache_control":{"type":"ephemeral"}}],
        "tools":[{"name":"echo","input_schema":{"type":"object"}}],
        "messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}]}]})
}

fn continuation(id: &str, stream: bool) -> Value {
    json!({"model":"local-model","max_tokens":64,"stream":stream,"thread":{"type":"continue","previous_message_id":id},
        "messages":[{"role":"user","content":"What do you remember?"}]})
}

async fn send(url: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/messages?beta=true"))
        .header("anthropic-beta", MESSAGE_THREADS_BETA)
        .header("x-api-key", "alice")
        .json(body)
        .send()
        .await
        .unwrap()
}

fn stream_id(body: &str) -> String {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "message_start")
        .unwrap()["message"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn create_continue_and_client_tool_results_work_in_both_modes() {
    for streaming in [false, true] {
        let (url, requests, _upstream, _gateway) = setup().await;
        let mut first = create(streaming);
        first["metadata"] = json!({"client_tool":true});
        let response = send(&url, &first).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        let id = if streaming {
            stream_id(&body)
        } else {
            serde_json::from_str::<Value>(&body).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_ne!(id, "upstream-id");
        let mut next = continuation(&id, !streaming);
        next["messages"][0]["content"] =
            json!([{"type":"tool_result","tool_use_id":"client-call","content":"success","is_error":false}]);
        let response = send(&url, &next).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("remembered 雪"));
        assert_eq!(
            send(&url, &next).await.json::<Value>().await.unwrap()["error"]["details"]["error_code"],
            "thread_already_continued"
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["system"], first["system"]);
        assert_eq!(requests[1]["tools"], first["tools"]);
        assert_eq!(requests[1]["messages"][0], first["messages"][0]);
        assert_eq!(
            requests[1]["messages"][1]["content"][0]["input"],
            json!({"text":"value"})
        );
        assert_eq!(requests[1]["messages"][2], next["messages"][0]);
    }
}

#[tokio::test]
async fn incompatible_and_wrong_owner_requests_never_reach_upstream() {
    let (url, requests, _upstream, _gateway) = setup().await;
    let first = send(&url, &create(false)).await.json::<Value>().await.unwrap();
    let mut next = continuation(first["id"].as_str().unwrap(), false);
    next["model"] = json!("different");
    let response = send(&url, &next).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["details"]["error_code"],
        "thread_fingerprint_mismatch"
    );
    next["model"] = json!("local-model");
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("anthropic-beta", MESSAGE_THREADS_BETA)
        .header("x-api-key", "bob")
        .json(&next)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["details"]["error_code"],
        "thread_not_found"
    );
    next["tools"] = Value::Null;
    assert_eq!(send(&url, &next).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(requests.lock().await.len(), 1);
}

#[tokio::test]
async fn claude_code_continues_with_full_or_inherited_fields_without_changing_the_prompt() {
    for streaming in [false, true] {
        for repeat_fields in [false, true] {
            let (url, requests, _upstream, _gateway) = setup().await;
            let send_cli = |body: Value| {
                let client = reqwest::Client::new();
                let url = url.clone();
                async move {
                    client
                        .post(format!("{url}/v1/messages"))
                        .header("anthropic-beta", format!("{MESSAGE_THREADS_BETA},{CLAUDE_CODE_BETA}"))
                        .header("x-api-key", "alice")
                        .json(&body)
                        .send()
                        .await
                        .unwrap()
                }
            };
            let metadata = json!({"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.317; cch=abc;"});
            let mut first = create(streaming);
            first["system"].as_array_mut().unwrap().insert(0, metadata.clone());
            let response = send_cli(first.clone()).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.text().await.unwrap();
            let id = if streaming {
                stream_id(&body)
            } else {
                serde_json::from_str::<Value>(&body).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let mut next = continuation(&id, streaming);
            next["system"] = if repeat_fields {
                next["tools"] = first["tools"].clone();
                first["system"].clone()
            } else {
                json!([metadata])
            };
            let mut invalid = next.clone();
            invalid["tools"] = json!([{"name":"changed","input_schema":{"type":"object"}}]);
            let response = send_cli(invalid).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["details"]["error_code"],
                "thread_fingerprint_mismatch"
            );
            let response = send_cli(next.clone()).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.text().await.unwrap();
            if streaming {
                assert!(body.contains("event: message_stop"));
                assert!(!body.contains("event: error"));
            }
            let requests = requests.lock().await;
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["system"], json!([first["system"][1]]));
            assert_eq!(requests[1]["system"], requests[0]["system"]);
            assert_eq!(requests[1]["tools"], first["tools"]);
            assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 3);
            assert_eq!(requests[1]["messages"][2], next["messages"][0]);
        }
    }
}

#[tokio::test]
async fn truncated_stream_never_publishes_a_checkpoint_and_parent_can_be_retried() {
    let (url, requests, _upstream, _gateway) = setup().await;
    let first = send(&url, &create(false)).await.json::<Value>().await.unwrap();
    let mut next = continuation(first["id"].as_str().unwrap(), true);
    next["metadata"] = json!({"truncate":true});
    let body = send(&url, &next).await.text().await.unwrap();
    assert!(body.contains("event: error"));
    assert!(!body.contains("event: message_stop"));
    let response = send(&url, &continuation(&stream_id(&body), false)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    next.as_object_mut().unwrap().remove("metadata");
    assert_eq!(send(&url, &next).await.status(), StatusCode::OK);
    assert_eq!(requests.lock().await.len(), 3);
}

#[tokio::test]
async fn count_tokens_rejects_threads_locally() {
    let (url, requests, _upstream, _gateway) = setup().await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/messages/count_tokens"))
        .json(&create(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(requests.lock().await.is_empty());
}

#[tokio::test]
async fn thread_continuation_retains_hidden_gateway_rounds_and_original_tools() {
    for stream in [false, true] {
        let (url, requests, _upstream, _gateway) = setup().await;
        let mut first = create(stream);
        first["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]);
        first["metadata"] = json!({"gateway_tool":true});
        let response = send(&url, &first).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(!body.contains("gateway-call"));
        let id = if stream {
            stream_id(&body)
        } else {
            serde_json::from_str::<Value>(&body).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let response = send(&url, &continuation(&id, stream)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains("remembered 雪"));
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        let messages = &requests[2]["messages"];
        assert_eq!(messages.as_array().unwrap().len(), 5);
        assert_eq!(messages[1]["content"][0]["id"], "gateway-call");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "gateway-call");
        assert_eq!(messages[2]["content"][0]["is_error"], false);
        assert!(
            messages[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .contains("local proof")
        );
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[4]["role"], "user");
        assert!(requests[2]["tools"][0].get("type").is_none());
        assert_eq!(requests[2]["tools"], requests[0]["tools"]);
    }
}

#[tokio::test]
async fn client_disconnect_releases_the_continuation_reservation() {
    let (url, requests, _upstream, _gateway) = setup().await;
    let first = send(&url, &create(false)).await.json::<Value>().await.unwrap();
    let mut next = continuation(first["id"].as_str().unwrap(), true);
    next["metadata"] = json!({"hang":true});
    let mut response = send(&url, &next).await;
    assert!(response.chunk().await.unwrap().is_some());
    drop(response);
    next.as_object_mut().unwrap().remove("metadata");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let response = send(&url, &next).await;
            if response.status() == StatusCode::OK {
                assert!(response.text().await.unwrap().contains("event: message_stop"));
                break;
            }
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["details"]["error_code"],
                "thread_continue_in_flight"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(requests.lock().await.len(), 3);
}

#[tokio::test]
async fn fallback_responses_are_delivered_but_not_retained() {
    for stream in [false, true] {
        let (url, requests, _upstream, _gateway) = setup().await;
        let mut first = create(stream);
        first["metadata"] = json!({"fallback":true});
        let response = send(&url, &first).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        let id = if stream {
            stream_id(&body)
        } else {
            serde_json::from_str::<Value>(&body).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let response = send(&url, &continuation(&id, false)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(requests.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn continuation_survives_gateway_and_execution_context_restart() {
    for stream in [false, true] {
        let (url, requests, upstream, mut gateway, database_url) = setup_database().await;
        let response = send(&url, &create(stream)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        let id = if stream {
            stream_id(&body)
        } else {
            serde_json::from_str::<Value>(&body).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let directory = gateway.1.take();
        gateway.0.abort();
        let _ = (&mut gateway.0).await;
        drop(gateway);
        // A fresh gateway and backend must recover history using only the database.
        let restarted_requests = Requests::default();
        let app = Router::new()
            .route("/v1/messages", post(infer))
            .with_state(Arc::clone(&restarted_requests));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_url = format!("http://{}", listener.local_addr().unwrap());
        let restarted_upstream = Server(
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
            None,
        );
        let mut config = common::test_config(&backend_url);
        config.db_url = Some(database_url);
        let mut state = common::test_state(&config);
        state.exec_ctx = Arc::new(
            agentic_core::executor::ExecutionContext::from_config(&config)
                .await
                .unwrap(),
        );
        let (restarted_url, handle) = common::spawn_gateway(state).await;
        let _restarted_gateway = Server(handle, directory);
        let response = send(&restarted_url, &continuation(&id, stream)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains("remembered 雪"));
        let requests = requests.lock().await;
        let recovered = restarted_requests.lock().await;
        assert_eq!(recovered[0]["messages"].as_array().unwrap().len(), 3);
        assert_eq!(recovered[0]["messages"][0], requests[0]["messages"][0]);
        assert_eq!(recovered[0]["messages"][1]["content"][0]["text"], "remembered 雪");
        drop(upstream);
        drop(restarted_upstream);
    }
}

#[tokio::test]
async fn persistence_failure_never_emits_successful_completion() {
    for stream in [false, true] {
        let (database_url, directory) = messages_database::database().await;
        let pool = agentic_core::storage::create_pool_with_schema(Some(&database_url))
            .await
            .unwrap();
        let failure_pool = Arc::clone(&pool);
        let requests = Requests::default();
        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, Json(request): Json<Value>| {
                let pool = Arc::clone(&failure_pool);
                let requests = Arc::clone(&requests);
                async move {
                    // The request was admitted, but its database becomes unavailable during inference.
                    sqlx::query("DROP TABLE message_threads")
                        .execute(pool.as_ref())
                        .await
                        .unwrap();
                    infer(State(requests), headers, Json(request)).await
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_url = format!("http://{}", listener.local_addr().unwrap());
        let _backend = Server(
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
            None,
        );
        let mut state = common::test_state(&common::test_config(&backend_url));
        Arc::make_mut(&mut state.exec_ctx).messages_threads =
            agentic_core::executor::messages_threads::MessagesThreadStore::new(pool);
        let (url, handle) = common::spawn_gateway(state).await;
        let _gateway = Server(handle, directory);
        let response = send(&url, &create(stream)).await;
        if stream {
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.text().await.unwrap();
            assert!(body.contains("event: error"), "{body}");
            assert!(!body.contains("event: message_stop"), "{body}");
        } else {
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(response.json::<Value>().await.unwrap()["type"], "error");
        }
    }
}
