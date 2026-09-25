//! Synthetic inference integration; reference cassettes belong to integration qualification.
use super::*;
#[path = "multi_agent_tests/root_completion.rs"]
mod root_completion;
#[path = "multi_agent_tests/shell_continuation.rs"]
mod shell_continuation;
use crate::executor::{
    ExecuteRequest,
    modes::{ConversationHandler, ResponseHandler},
};
use crate::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use crate::types::io::{FunctionToolResultMessage, InputItem, MessagePhase, ResponsesInput, ToolCallOutput};
use crate::types::request_response::ContextManagement;
use crate::types::tools::{FunctionToolParam, ResponsesTool};
use crate::utils::common::uuid7_str;
use axum::body::Body;
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use either::Either;
use futures::StreamExt;
use root_completion::{reasoning_only_recovery_output, root_completion_output};
use serde_json::{Value, json};
use shell_continuation::shell_review_output;
use std::sync::Arc;
use std::{collections::HashMap, fmt::Write};
use tokio::sync::{Barrier, Semaphore};

#[allow(clippy::needless_pass_by_value)] // Convenient owned JSON test fixtures.
fn function(call_id: &str, name: &str, arguments: Value) -> Value {
    json!({"type":"function_call","id":uuid7_str("fc_"),"call_id":call_id,
        "name":name,"arguments":arguments.to_string(),"status":"completed"})
}

fn message(text: &str) -> Value {
    json!({"type":"message","id":uuid7_str("msg_"),"role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}

// Check the actual upstream request on initial execution and stored continuation.
fn assert_child_assignment(request: &Value, child: &str) {
    let guidance = request["input"].as_array().unwrap().last().unwrap();
    assert_eq!(guidance["role"], "developer");
    let instructions = guidance["content"].as_str().unwrap();
    assert!(instructions.contains(&format!("You are `/root/{child}`")));
    assert!(instructions.contains("Your parent is `/root`"));
    assert!(instructions.contains("2 of 2 slots are occupied; 0 are free right now"));
    assert!(instructions.contains("call spawn_agent at most 0 times"));
    assert!(instructions.contains("You have no direct children"));
    assert!(instructions.contains("Never hand your entire assignment to another agent"));
    assert!(instructions.contains("Your parent's delegation request is already fulfilled by your existence"));
    assert!(instructions.ends_with(&format!("Your current assignment:\nassess {child}")));
    assert_eq!(
        request["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "developer")
            .count(),
        1,
        "guidance must not accumulate or be forked"
    );
    assert!(instructions.contains("not actions you performed or agents you spawned"));
    assert!(instructions.contains("including you, your ancestors and your siblings"));
    let input = request["input"].as_array().unwrap();
    // Forked context is preserved, while a separate assignment tells the child
    // which work it owns. Disabling forks is not the fix.
    assert!(input.iter().any(|item| {
        item["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Compare proposals"))
    }));
    let task = input
        .iter()
        .rev()
        .find_map(|item| {
            item["content"]
                .as_str()
                .filter(|text| text.starts_with("Message Type: NEW_TASK"))
        })
        .expect("child receives a task after inherited history");
    assert!(task.contains(&format!("You are /root/{child}.")));
    assert!(task.ends_with(&format!("Payload:\nassess {child}")));
    assert!(
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "spawn_agent")
    );
}

async fn setup() -> (Arc<ExecutionContext>, tokio::task::JoinHandle<()>) {
    setup_with_gate(None).await
}

async fn setup_with_gate(gate: Option<Arc<Semaphore>>) -> (Arc<ExecutionContext>, tokio::task::JoinHandle<()>) {
    let barrier = Arc::new(Barrier::new(2));
    let app = Router::new().route("/v1/responses", post(move |Json(request): Json<Value>| {
        let barrier = barrier.clone();
        let gate = gate.clone();
        async move {
            assert!(request.get("multi_agent").is_none());
            assert!(request["input"].as_array().unwrap().iter().all(|item| !matches!(
                item["type"].as_str(), Some("multi_agent_call" | "multi_agent_call_output" | "agent_message"))));
            let input = request["input"].as_array().unwrap();
            let instructions = input.last().and_then(|item| item["content"].as_str()).unwrap_or("");
            let child = if instructions.contains("You are `/root/alpha`") { Some("alpha") }
                else if instructions.contains("You are `/root/beta`") { Some("beta") } else { None };
            let summarizing = input.last().is_some_and(|item| item["content"].as_str().is_some_and(|text| text.starts_with("You are performing a CONTEXT CHECKPOINT COMPACTION")));
            let output = if summarizing {
                vec![message("Context summary")]
            } else if let Some(output) = root_completion_output(&request) {
                output
            } else if let Some(output) = reasoning_only_recovery_output(&request) {
                output
            } else if let Some(output) = shell_review_output(&request) {
                output
            } else if input.iter().any(|item| item["content"] == "simple compaction test") {
                vec![message("finished")]
            } else if let Some(child) = child {
                assert_child_assignment(&request, child);
                let call_id = format!("proposal_{child}");
                if input.iter().any(|item| item["type"] == "function_call_output" && item["call_id"] == call_id) {
                    vec![message(&format!("{child} assessment"))]
                } else {
                    barrier.wait().await; // Fails if children are executed sequentially.
                    vec![function(&call_id, "get_proposal", json!({"proposal": child}))]
                }
            } else if input.iter().any(|item| item["type"] == "function_call_output" && item["call_id"] == "spawn_alpha") {
                if input.iter().filter(|item| item["content"].as_str().is_some_and(|text| text.contains("Message Type: FINAL_ANSWER"))).count() == 2 {
                    vec![message("combined answer")]
                } else {
                    vec![function(&format!("wait_children_{}", input.len()), "wait_agent", json!({"timeout_ms":10000}))]
                }
            } else {
                vec![function("spawn_alpha", "spawn_agent", json!({"task_name":"alpha","message":"assess alpha","fork_turns":"all"})),
                    function("spawn_beta", "spawn_agent", json!({"task_name":"beta","message":"assess beta","fork_turns":"all"}))]
            };
            let payload = json!({"id":"upstream", "object":"response", "created_at":0,"model":"test","status":"completed",
                "output":output,"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3},
                "incomplete_details":null,"error":null,"previous_response_id":null,"conversation_id":null,"instructions":null});
            if request["stream"] == true {
                if let Some(gate) = gate.filter(|_| child.is_some()) {
                    let events = upstream_events(&payload);
                    let start = events.rfind("data: ").unwrap();
                    let prefix = events[..start].to_owned();
                    let terminal = events[start..].to_owned();
                    let body = futures::stream::once(async move { Ok::<_, std::convert::Infallible>(prefix) })
                        .chain(futures::stream::once(async move {
                            gate.acquire().await.unwrap().forget();
                            Ok(terminal)
                        }));
                    return ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response();
                }
                ([("content-type", "text/event-stream")], upstream_events(&payload)).into_response()
            } else {
                Json(payload).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let exec = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(pool.clone())),
        ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        format!("http://{address}"),
    );
    (Arc::new(exec), server)
}

#[tokio::test]
async fn automatic_compaction_is_attributed_and_usage_is_counted_once() {
    let (exec, server) = setup().await;
    let request = RequestPayload {
        model: "test".into(),
        store: true,
        input: ResponsesInput::Text("simple compaction test".into()),
        multi_agent: Some(MultiAgentConfig {
            enabled: true,
            max_concurrent_subagents: Some(3),
        }),
        context_management: Some(vec![ContextManagement {
            type_: "compaction".into(),
            compact_threshold: Some(1),
        }]),
        ..Default::default()
    };
    let result = tokio::time::timeout(Duration::from_secs(10), response(request, exec)).await;
    server.abort();
    let result = result.unwrap();
    assert_eq!(result.usage.unwrap().total_tokens, 6);
    assert!(matches!(result.output.first(), Some(OutputItem::Compaction(item))
        if item.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root")));
    assert_eq!(result.output.len(), 2);
}

#[tokio::test]
async fn child_deltas_arrive_before_upstream_round_completion() {
    let gate = Arc::new(Semaphore::new(0));
    let (exec, server) = setup_with_gate(Some(gate.clone())).await;
    let work = async {
        let request = RequestPayload {
            model: "test".into(),
            store: true,
            stream: true,
            input: ResponsesInput::Text("Compare proposals".into()),
            multi_agent: Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(2),
            }),
            ..Default::default()
        };
        let Either::Right(mut stream) = ExecuteRequest::new(request, exec).run().await.unwrap() else {
            panic!("expected SSE");
        };
        let mut seen = HashSet::new();
        while seen.len() < 2 {
            let chunk = stream.next().await.expect("stream ended before child deltas");
            for line in chunk.lines().filter_map(|line| line.strip_prefix("data: ")) {
                let frame: Value = serde_json::from_str(line).unwrap();
                assert_ne!(frame["type"], "error", "{frame}");
                if frame["type"] == "response.function_call_arguments.delta" {
                    seen.insert(frame["agent"]["agent_name"].as_str().unwrap().to_owned());
                }
            }
        }
        assert!(seen.contains("/root/alpha") && seen.contains("/root/beta"));
        gate.add_permits(2);
        let mut completed = false;
        while let Some(chunk) = stream.next().await {
            assert!(!chunk.contains("event: error"), "{chunk}");
            completed |= chunk.contains("event: response.completed");
        }
        assert!(completed);
    };
    let result = tokio::time::timeout(Duration::from_secs(15), work).await;
    server.abort();
    result.unwrap();
}

async fn response(request: RequestPayload, exec: Arc<ExecutionContext>) -> ResponsePayload {
    match ExecuteRequest::new(request, exec).run().await.unwrap() {
        Either::Left(payload) => payload,
        Either::Right(mut stream) => {
            let mut terminal = None;
            let mut sequence = 0;
            let mut done = BTreeMap::new();
            while let Some(chunk) = stream.next().await {
                for data in chunk.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    if data == "[DONE]" {
                        continue;
                    }
                    let event: Value = serde_json::from_str(data).unwrap();
                    assert_eq!(event["sequence_number"], sequence, "{event}");
                    sequence += 1;
                    assert_ne!(event["type"], "error", "{event}");
                    if event["type"] == "response.output_item.done" {
                        done.insert(event["output_index"].as_u64().unwrap(), event["item"].clone());
                    }
                    if event["type"] == "response.completed" {
                        assert!(event.get("agent").is_none());
                        terminal = Some(serde_json::from_value::<ResponsePayload>(event["response"].clone()).unwrap());
                    }
                }
            }
            let terminal = terminal.unwrap();
            assert_eq!(
                done.into_values().collect::<Vec<_>>(),
                serde_json::to_value(&terminal.output)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .clone()
            );
            terminal
        }
    }
}

fn upstream_events(payload: &Value) -> String {
    let mut events =
        vec![json!({"type":"response.created","response":{"id":"upstream","status":"in_progress","output":[]}})];
    for (index, item) in payload["output"].as_array().unwrap().iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item["type"] == "function_call" {
            added["arguments"] = json!("");
        } else {
            added["content"] = json!([]);
        }
        events.push(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        if item["type"] == "function_call" {
            events.push(json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":item["id"],"delta":item["arguments"]}));
            events.push(json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":item["id"],"name":item["name"],"arguments":item["arguments"]}));
        } else {
            let part = &item["content"][0];
            events.push(json!({"type":"response.content_part.added","output_index":index,"item_id":item["id"],"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}));
            events.push(json!({"type":"response.output_text.delta","output_index":index,"item_id":item["id"],"content_index":0,"delta":part["text"]}));
            events.push(json!({"type":"response.output_text.done","output_index":index,"item_id":item["id"],"content_index":0,"text":part["text"]}));
            events.push(json!({"type":"response.content_part.done","output_index":index,"item_id":item["id"],"content_index":0,"part":part}));
        }
        events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    events.push(json!({"type":"response.completed","response":payload}));
    let mut body = String::new();
    for (sequence, event) in events.iter_mut().enumerate() {
        event["sequence_number"] = json!(sequence);
        write!(body, "data: {event}\n\n").unwrap();
    }
    body
}

#[tokio::test]
async fn concurrent_children_pause_and_resume_from_durable_tree_json_and_sse() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let request = RequestPayload {
                model: "test".into(),
                store: true,
                stream,
                input: ResponsesInput::Text("Compare proposals using two agents".into()),
                multi_agent: Some(MultiAgentConfig {
                    enabled: true,
                    max_concurrent_subagents: Some(2),
                }),
                tools: Some(vec![ResponsesTool::Function(FunctionToolParam {
                    name: "get_proposal".try_into().unwrap(),
                    description: None,
                    parameters: Some(json!({"type":"object","properties":{"proposal":{"type":"string"}}})),
                    strict: None,
                    defer_loading: None,
                    extra: HashMap::default(),
                })]),
                ..Default::default()
            };
            let first = response(request, exec.clone()).await;
            let calls = first
                .output
                .iter()
                .filter_map(|item| match item {
                    OutputItem::FunctionCall(call) => Some(call),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 2);
            assert_ne!(calls[0].agent, calls[1].agent);
            assert_eq!(first.usage.unwrap().total_tokens, 12);
            let next = RequestPayload {
                model: "test".into(),
                store: true,
                stream,
                previous_response_id: Some(first.id.clone()),
                input: ResponsesInput::Items(
                    calls
                        .iter()
                        .map(|call| {
                            InputItem::FunctionCallOutput(FunctionToolResultMessage {
                                call_id: call.call_id.clone(),
                                output: ToolCallOutput::Text("proposal details".into()),
                            })
                        })
                        .collect(),
                ),
                ..Default::default()
            };
            let second = response(next.clone(), exec.clone()).await;
            assert!(second.output.iter().any(|item| matches!(item, OutputItem::Message(message)
                if message.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root") && message.phase == Some(MessagePhase::FinalAnswer))));
            assert!((9..=15).contains(&second.usage.unwrap().total_tokens));
            // Branch from the same parent: no mutations leak from the first continuation.
            let branch = response(next, exec.clone()).await;
            assert_ne!(branch.id, second.id);
            assert!((9..=15).contains(&branch.usage.unwrap().total_tokens));
        };
        let result = tokio::time::timeout(Duration::from_secs(15), work).await;
        server.abort();
        result.unwrap();
    }
}

#[test]
fn forks_do_not_duplicate_pending_parent_calls() {
    let history: Vec<InputItem> = serde_json::from_value(json!([
        {"role":"user","content":"context"},
        {"type":"function_call","call_id":"open","name":"get_proposal","arguments":"{}"},
        {"type":"function_call","call_id":"closed","name":"get_proposal","arguments":"{}"},
        {"type":"function_call_output","call_id":"closed","output":"value"}
    ]))
    .unwrap();
    let forked = fork_history(&history, "all").unwrap();
    assert_eq!(forked.len(), 3);
    assert!(fork_history(&history, "none").unwrap().is_empty());
    assert!(fork_history(&history, "0").is_err());
}
