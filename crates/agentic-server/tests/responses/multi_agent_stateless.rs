//! Replay recorded dependencies through the real HTTP gateway with live client continuations.
#[path = "multi_agent_dependencies.rs"]
mod dependencies;

use agentic_core::utils::common::deserialize_from_value;
use std::collections::HashMap;
use std::fs::read_to_string;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::common::{spawn_gateway, test_config, test_state};
use crate::multi_agent_contract::{ComparisonPolicy, RecordedSession, assert_multi_agent_contract};
use crate::support;
use agentic_core::events::normalize_sse_line;
use agentic_core::executor::request::RequestContext;
use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler, UpstreamBody, decode_upstream};
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_core::tool::WebSearchHandler;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use dependencies::Dependencies;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::time::timeout;

const DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../agentic-server-core/tests/cassettes/store_false"
);

fn terminal(response: &Value) -> Value {
    if let Some(body) = response.get("body") {
        return body.clone();
    }
    response["sse"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .flat_map(str::lines)
        .filter_map(normalize_sse_line)
        .find(|frame| {
            frame
                .wire
                .rest
                .get("response")
                .is_some_and(|response| response["status"] == "completed")
        })
        .expect("completed response")
        .wire
        .rest["response"]
        .clone()
}

// Match fixture commands exactly, but answer the calls emitted by this replay.
fn shell_outputs(output: &Value, results: &HashMap<String, Value>) -> Vec<Value> {
    let mut client_outputs = Vec::new();
    for call in output
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "shell_call")
    {
        let outputs: Vec<_> = call["action"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| {
                results
                    .get(command.as_str().unwrap())
                    .expect("recorded shell command")
                    .clone()
            })
            .collect();
        let mut result = json!({"type":"shell_call_output", "call_id":call["call_id"], "output":outputs});
        if let Some(limit) = call["action"].get("max_output_length").filter(|limit| !limit.is_null()) {
            result["max_output_length"] = limit.clone();
        }
        client_outputs.push(result);
    }
    client_outputs
}

async fn replay_mode(mode: &str) {
    let model = Dependencies::start(&format!("{DIR}/multi-agent-store-false-model-dependencies-{mode}.yaml")).await;
    let search = Dependencies::start(&format!(
        "{DIR}/multi-agent-store-false-search-dependencies-{mode}.yaml"
    ))
    .await;
    let mcp = Dependencies::start(&format!("{DIR}/multi-agent-store-false-mcp-dependencies-{mode}.yaml")).await;
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let client = Arc::new(Client::new());
    let config = test_config(&model.url);
    let mut state = test_state(&config);
    state.exec_ctx = Arc::new(
        ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(pool.clone())),
            ResponseHandler::new(ResponseStore::new(pool.clone())),
            client.clone(),
            model.url.clone(),
        )
        .with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
            client.clone(),
            "recorded-search-key".into(),
            &search.url,
        ))),
    );
    let (url, gateway) = spawn_gateway(state).await;
    let path = format!("{DIR}/multi-agent-store-false-gateway-mixed-tools-Qwen-Qwen3.8-27B-FP8-{mode}.yaml");
    let capture: Value = serde_yaml::from_str(&read_to_string(path).unwrap()).unwrap();
    let shell_results = model.shell_results().await;
    let mut opening = capture["turns"][0]["request"]["body"].clone();
    // The paired model trace owns inference settings; a refreshed public recording may use a different budget.
    opening["max_output_tokens"] = json!(model.max_output_tokens().await);
    let mut actual_turns = Vec::new();
    let mut history = opening["input"].as_array().unwrap().clone();
    for continuation in 0..=10 {
        let mut request = opening.clone();
        request["input"] = json!(history);
        for tool in request["tools"].as_array_mut().unwrap() {
            if tool["type"] == "mcp" {
                tool["server_url"] = json!(format!("{}/openai/tiktoken", mcp.url));
            }
        }
        assert_eq!(request["store"], false);
        assert!(request.get("previous_response_id").is_none());
        history = request["input"].as_array().unwrap().clone();
        let response = client
            .post(format!("{url}/v1/responses"))
            .bearer_auth("test-key")
            .header("OpenAI-Beta", "responses_multi_agent=v1")
            .json(&request)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        model.assert_no_errors().await;
        search.assert_no_errors().await;
        mcp.assert_no_errors().await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let response = if mode == "streaming" {
            json!({"sse": text.split_inclusive('\n').collect::<Vec<_>>()})
        } else {
            json!({"body": serde_json::from_str::<Value>(&text).unwrap()})
        };
        let payload = terminal(&response);
        let typed: ResponsePayload = deserialize_from_value(payload.clone()).unwrap();
        let retrieval = client
            .get(format!("{url}/v1/responses/{}", typed.id))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(retrieval.status(), StatusCode::NOT_FOUND);
        let client_outputs = shell_outputs(&payload["output"], &shell_results);
        history.extend_from_slice(payload["output"].as_array().unwrap());
        actual_turns.push(json!({"request":{"path":"/v1/responses","body":request},"response":response}));
        if client_outputs.is_empty() {
            break;
        }
        assert!(continuation < 10, "client-tool continuation limit exceeded");
        history.extend(client_outputs);
    }
    assert_reference_contract(mode, &actual_turns).await;
    for table in ["responses", "items", "conversations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(count, 0, "store:false wrote {table}");
    }
    model.assert_consumed().await;
    search.assert_consumed().await;
    mcp.assert_consumed().await;
    gateway.abort();
}

async fn assert_reference_contract(mode: &str, actual_turns: &[Value]) {
    for turn in actual_turns {
        let payload = terminal(&turn["response"]);
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["store"], false);
        let output = payload["output"].as_array().unwrap();
        assert!(
            !output.iter().any(|item| item["type"] == "compaction"),
            "this scenario does not trigger automatic compaction"
        );
        assert!(
            !output.iter().any(|item| item["type"] == "agent_message"
                && item["author"] == "/root"
                && item["recipient"] == "/root")
        );
        if let Some(chunks) = turn["response"]["sse"].as_array() {
            let stream: String = chunks.iter().map(|chunk| chunk.as_str().unwrap()).collect();
            let original_request: RequestPayload = deserialize_from_value(turn["request"]["body"].clone()).unwrap();
            let context = RequestContext {
                enriched_request: original_request.clone(),
                original_request,
                multi_agent_tree: None,
                new_input_items: Vec::new(),
                response_id: "resp_strict_replay".to_owned(),
                conversation_id: None,
                conversation_version: None,
                continuation: None,
            };
            decode_upstream(context, UpstreamBody::Sse(&stream))
                .await
                .expect("live gateway SSE must satisfy strict lifecycle validation");
        }
    }
    let cassette: support::Cassette = deserialize_from_value(json!({"turns":actual_turns})).unwrap();
    let actual = RecordedSession::from_cassette(&cassette).unwrap();
    let reference = RecordedSession::load(Path::new(&format!(
        "{DIR}/multi-agent-store-false-openai-reference-mixed-tools-gpt-5.6-sol-{mode}.yaml"
    )))
    .unwrap();
    assert_multi_agent_contract(
        &reference,
        &actual,
        &ComparisonPolicy {
            require_reference_tool_kinds: true,
            minimum_delegated_agents: Some(3),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn mixed_tools_store_false_json_runs_through_gateway() {
    timeout(Duration::from_secs(60), replay_mode("nonstreaming"))
        .await
        .expect("gateway replay timed out");
}

#[tokio::test]
async fn mixed_tools_store_false_sse_runs_through_gateway() {
    timeout(Duration::from_secs(60), replay_mode("streaming"))
        .await
        .expect("gateway replay timed out");
}
