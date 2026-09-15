//! Provider-neutral tests use existing vLLM recordings. The Dynamo acceptance test
//! replays the checked-in recordings captured from a real GPU-backed Dynamo worker.
mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentic_core::executor::{
    ConversationHandler, ExecutionContext, MessagesRequestContext, MessagesUpstream, ResponseHandler,
    run_messages_loop, run_messages_stream,
};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::{
    GatewayExecutor, ToolError, ToolHandler, ToolOutput, ToolRegistry, ToolType, WebSearchHandler,
};
use agentic_core::types::io::FunctionTool;
use agentic_core::types::messages::{ToolParam, registry_tools};
use agentic_core::types::tools::WebSearchToolParam;
use axum::{Router, routing::post};
use futures::{FutureExt, StreamExt};
use serde_json::Value;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes");

struct RecordedTool {
    expected_input: Value,
    output: String,
    calls: Mutex<usize>,
}

impl ToolHandler for RecordedTool {
    type ToolParams = WebSearchToolParam;
    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }
    fn validate(&self, _: &WebSearchToolParam) -> Result<(), ToolError> {
        Ok(())
    }
    fn normalize(&self, params: &WebSearchToolParam) -> Vec<FunctionTool> {
        WebSearchHandler::with_api_key(Arc::new(reqwest::Client::new()), "unused".into(), "http://unused")
            .normalize(params)
    }
}

impl GatewayExecutor for RecordedTool {
    type ExecutionParams = WebSearchToolParam;
    fn execute(
        &self,
        call_id: &str,
        name: &str,
        arguments: &str,
        _: &WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        assert_eq!(name, "web_search");
        assert_eq!(serde_json::from_str::<Value>(arguments).unwrap(), self.expected_input);
        *self.calls.lock().unwrap() += 1;
        let output = ToolOutput {
            call_id: call_id.to_owned(),
            output: self.output.clone(),
        };
        Box::pin(async move { Ok(output) })
    }
}

async fn replay(path: &str, streaming: bool) {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("real recording required at {path}: {error}"));
    let doc: Value = serde_yaml::from_str(&raw).unwrap();
    let turns = doc["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    let expected = expected_requests(turns, streaming && path.contains("/messages/"));
    let history = expected[1]["messages"].as_array().unwrap();
    let call = history[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap();
    let tool = Arc::new(RecordedTool {
        expected_input: call["input"].clone(),
        output: history[2]["content"][0]["content"].as_str().unwrap().to_owned(),
        calls: Mutex::new(0),
    });
    let responses: Vec<String> = turns
        .iter()
        .map(|turn| {
            assert_eq!(turn["response"]["status_code"], 200);
            if streaming {
                turn["response"]["sse"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap())
                    .collect()
            } else {
                turn["response"]["body"].to_string()
            }
        })
        .collect();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = replay_router(responses, captured, streaming);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Abort and join even when the bounded run times out. No detached replay server.
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        std::panic::AssertUnwindSafe(async {
            let pool = support::setup_pool().await;
            let ctx = Arc::new(
                ExecutionContext::new(
                    ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
                    ResponseHandler::new(ResponseStore::new(pool)),
                    Arc::new(reqwest::Client::new()),
                    base,
                )
                .with_gateway_executor(Arc::clone(&tool)),
            );
            let params: Vec<ToolParam> = serde_json::from_value(expected[0]["tools"].clone()).unwrap();
            let mut tools = registry_tools(Some(&params), &ctx.messages_gateway_tools);
            let registry = ToolRegistry::build_with_handlers(&mut tools, &mut ctx.gateway_executors.clone())
                .await
                .unwrap();
            let upstream = MessagesUpstream::new(&ctx.llm_base_url, None, http::HeaderMap::new());
            if streaming {
                let request = MessagesRequestContext::from_value(expected[0].clone()).unwrap();
                let response = run_messages_stream(request, Arc::new(registry), ctx, upstream)
                    .await
                    .unwrap();
                let raw = response.body.collect::<Vec<_>>().await.join("");
                assert_public_stream(&raw, &turns[1]);
            } else {
                let request = MessagesRequestContext::from_value(expected[0].clone()).unwrap();
                let response = run_messages_loop(request, &registry, &ctx, &upstream).await.unwrap();
                assert_eq!(response.body["stop_reason"], "end_turn");
                assert_eq!(response.body["content"], turns[1]["response"]["body"]["content"]);
            }
        })
        .catch_unwind(),
    )
    .await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    if let Err(panic) = outcome.expect("Messages replay deadline") {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(*tool.calls.lock().unwrap(), 1);
    let actual = requests.lock().unwrap();
    assert_eq!(actual.len(), 2);
    for (actual, expected) in actual.iter().zip(&expected) {
        for field in ["model", "messages", "tools", "stream", "max_tokens"] {
            assert_eq!(actual[field], expected[field], "upstream {field} must match recording");
        }
    }
}

#[tokio::test]
async fn messages_replay_preparation_nonstreaming() {
    replay(
        &format!("{ROOT}/messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml"),
        false,
    )
    .await;
}

#[tokio::test]
async fn messages_replay_preparation_streaming() {
    replay(
        &format!("{ROOT}/messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"),
        true,
    )
    .await;
}

#[tokio::test]
async fn dynamo_messages_recorded_acceptance() {
    for (suffix, streaming) in [("nonstreaming", false), ("streaming", true)] {
        replay(
            &format!("{ROOT}/dynamo/dynamo-messages-web-search-openai-gpt-oss-20b-{suffix}.yaml"),
            streaming,
        )
        .await;
    }
}

fn expected_requests(turns: &[Value], legacy_streaming: bool) -> Vec<Value> {
    let mut expected: Vec<Value> = turns.iter().map(|t| t["request"]["body"].clone()).collect();
    // The gateway explicitly emits the protocol default; the recorder omits it.
    expected[1]["messages"][2]["content"][0]["is_error"] = Value::Bool(false);
    // This legacy vLLM fixture predates signature_delta support in the recorder.
    // Recover its expectation from the captured wire event, never change the cassette.
    // Real Dynamo acceptance compares its recorded history without this exception.
    if legacy_streaming {
        for line in turns[0]["response"]["sse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s.as_str().unwrap().lines())
        {
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(event) = serde_json::from_str::<Value>(data.trim()) {
                    if event["delta"]["type"] == "signature_delta" {
                        let index = usize::try_from(event["index"].as_u64().unwrap()).unwrap();
                        expected[1]["messages"][1]["content"][index]["signature"] = event["delta"]["signature"].clone();
                    }
                }
            }
        }
    }
    expected
}

fn assert_public_stream(raw: &str, final_turn: &Value) {
    let events: Vec<Value> = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|s| serde_json::from_str(s.trim()).ok())
        .collect();
    for kind in ["message_start", "message_stop", "message_delta"] {
        assert_eq!(events.iter().filter(|e| e["type"] == kind).count(), 1, "{kind}");
    }
    let starts: Vec<&Value> = events.iter().filter(|e| e["type"] == "content_block_start").collect();
    assert!(!starts.is_empty());
    for (index, event) in starts.iter().enumerate() {
        assert_eq!(event["index"].as_u64().unwrap(), u64::try_from(index).unwrap());
        assert_ne!(event["content_block"]["type"], "tool_use");
    }
    let terminal = events.iter().find(|e| e["type"] == "message_delta").unwrap();
    assert_eq!(terminal["delta"]["stop_reason"], "end_turn");
    let text: String = events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .map(|e| e["delta"]["text"].as_str().unwrap())
        .collect();
    assert!(!text.trim().is_empty());
    let upstream_text: String = final_turn["response"]["sse"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s.as_str().unwrap().lines())
        .filter_map(|s| s.strip_prefix("data:"))
        .filter_map(|s| serde_json::from_str::<Value>(s.trim()).ok())
        .filter(|e| e["delta"]["type"] == "text_delta")
        .map(|e| e["delta"]["text"].as_str().unwrap().to_owned())
        .collect();
    assert!(text.ends_with(&upstream_text));
}

fn replay_router(responses: Vec<String>, captured: Arc<Mutex<Vec<Value>>>, streaming: bool) -> Router {
    Router::new().route(
        "/v1/messages",
        post(move |axum::Json(body): axum::Json<Value>| {
            let mut captured = captured.lock().unwrap();
            let index = captured.len();
            captured.push(body);
            let response = responses.get(index).cloned();
            async move {
                match response {
                    Some(body) => (
                        http::StatusCode::OK,
                        [(
                            "content-type",
                            if streaming {
                                "text/event-stream"
                            } else {
                                "application/json"
                            },
                        )],
                        body,
                    ),
                    None => (
                        http::StatusCode::INTERNAL_SERVER_ERROR,
                        [("content-type", "text/plain")],
                        "replay exhausted".into(),
                    ),
                }
            }
        }),
    )
}
