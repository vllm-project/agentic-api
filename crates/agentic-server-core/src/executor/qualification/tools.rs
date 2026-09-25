//! The recorded function is bound to a deterministic local MCP executor.
//! Provider response bytes stay unchanged; MCP normalization is local evidence,
//! not a claim that this declaration has been qualified against a live provider.

use super::support::*;
use crate::executor::{rehydrate_conversation, rehydrate_in_session};
use crate::tool::{GatewayExecutorRegistration, McpDiscoveredHandler, McpHandler, mcp::McpClient};
use crate::types::{RequestPayload, io::OutputItem, tools::McpDiscoveredToolParam};
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;

struct LookupServer {
    client: Arc<McpClient>,
    calls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl LookupServer {
    async fn new() -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/mcp",
                post(
                    |State(calls): State<Arc<AtomicUsize>>, Json(message): Json<Value>| async move {
                        let result = match message["method"].as_str() {
                            Some("initialize") => json!({
                                "protocolVersion":"2025-06-18", "capabilities":{"tools":{}},
                                "serverInfo":{"name":"offline-lookup", "version":"1"}
                            }),
                            Some("notifications/initialized") => return StatusCode::ACCEPTED.into_response(),
                            Some("tools/call") => {
                                assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0, "one tool call");
                                assert_eq!(message["params"]["name"], "lookup_code");
                                assert_eq!(message["params"]["arguments"]["label"], "ALPHA47");
                                json!({"content":[{"type":"text", "text":"ORCHID-47"}], "isError":false})
                            }
                            _ => return StatusCode::BAD_REQUEST.into_response(),
                        };
                        Json(json!({"jsonrpc":"2.0", "id":message["id"], "result":result})).into_response()
                    },
                ),
            )
            .with_state(Arc::clone(&calls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            McpClient::connect(&format!("http://{address}/mcp"), None),
        )
        .await
        .unwrap()
        .unwrap();
        Self {
            client: Arc::new(client),
            calls,
            task,
        }
    }

    fn registration(&self, turn: &Turn) -> GatewayExecutorRegistration {
        let function = &turn.request.body["tools"][0];
        GatewayExecutorRegistration::Mcp {
            server_label: "fixture".to_owned(),
            handlers: vec![McpDiscoveredHandler {
                param: McpDiscoveredToolParam {
                    server_label: "fixture".to_owned(), tool_name: "lookup_code".to_owned(),
                    internal_name: "lookup_code".to_owned(),
                    tool: serde_json::from_value(json!({"name":"lookup_code", "description":function["description"], "inputSchema":function["parameters"]})).unwrap(),
                },
                handler: Arc::new(McpHandler::tool_call(Arc::clone(&self.client))),
            }],
        }
    }

    async fn stop(mut self) {
        self.task.abort();
        assert!((&mut self.task).await.unwrap_err().is_cancelled());
    }
}

impl Drop for LookupServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn pinned_execution_gateway_tool_rounds_preserve_reasoning_order_in_storage_and_sessions() {
    for streaming in [false, true] {
        for (transient, conversation) in [(false, false), (true, false), (false, true)] {
            let capture = capture("function", streaming);
            let mut fixture = Fixture::new(capture.turns[..2].iter().map(|turn| turn.response.clone())).await;
            let tool = LookupServer::new().await;
            Arc::get_mut(&mut fixture.context)
                .unwrap()
                .gateway_executors
                .insert(tool.registration(&capture.turns[0]));
            let group = group();
            let session = group.new_session().unwrap();
            let mut initial = request(&capture.turns[0], !transient);
            if conversation {
                initial.conversation_id = Some(fixture.context.conv_handler.create().await.unwrap().conversation_id);
            }
            let conversation_id = initial.conversation_id.clone();
            initial.tools = Some(
                serde_json::from_value(json!([{"type":"mcp", "server_label":"fixture", "require_approval":"never"}]))
                    .unwrap(),
            );
            let result = collect(fixture.run(initial, transient.then_some(&session)).await.unwrap()).await;
            assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                result
                    .output
                    .iter()
                    .filter(|item| matches!(item, OutputItem::Reasoning(_)))
                    .count(),
                2
            );
            assert_eq!(
                result
                    .output
                    .iter()
                    .filter(|item| matches!(item, OutputItem::McpCall(_)))
                    .count(),
                1
            );
            assert!(
                !result
                    .output
                    .iter()
                    .any(|item| matches!(item, OutputItem::FunctionCall(_)))
            );
            let sent = fixture.requests().await;
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[1]["store"], false);
            assert_input(&capture.turns[1].request.body["input"], &sent[1]["input"]);
            let mut followup: RequestPayload = serde_json::from_value(json!({
                "model":PROFILE.model(), "input":"audit", "store":!transient, "previous_response_id":result.id
            }))
            .unwrap();
            if conversation {
                followup.previous_response_id = None;
                followup.conversation_id = conversation_id;
            }
            let context = if transient {
                rehydrate_in_session(followup, &fixture.context, &session)
                    .await
                    .unwrap()
            } else {
                rehydrate_conversation(followup, &fixture.context).await.unwrap()
            };
            let projected = serde_json::to_value(
                context
                    .enriched_request
                    .to_upstream_request(false)
                    .unwrap()
                    .with_opaque_replay(),
            )
            .unwrap();
            crate::executor::replay::preflight_inference(&fixture.context, &context.enriched_request, Some(AUTH))
                .unwrap();
            let prefix_len = sent[1]["input"].as_array().unwrap().len();
            assert_eq!(
                projected["input"].as_array().unwrap().len(),
                prefix_len + 3,
                "final reasoning, assistant message and new input appear exactly once"
            );
            assert_input(
                &sent[1]["input"],
                &Value::Array(projected["input"].as_array().unwrap()[..prefix_len].to_vec()),
            );
            drop(context);
            assert_eq!(fixture.row_count().await, i64::from(!transient));
            fixture.stop().await;
            tool.stop().await;
        }
    }
}

#[tokio::test]
async fn pinned_execution_second_round_mismatch_does_not_persist_successful_tool() {
    use super::failures::{Fault, assert_failed, fault};
    for streaming in [false, true] {
        let capture = capture("function", streaming);
        let mut fixture = Fixture::new([
            capture.turns[0].response.clone(),
            fault(capture.turns[1].response.clone(), Fault::WrongModel),
        ])
        .await;
        let tool = LookupServer::new().await;
        Arc::get_mut(&mut fixture.context)
            .unwrap()
            .gateway_executors
            .insert(tool.registration(&capture.turns[0]));
        let mut initial = request(&capture.turns[0], true);
        initial.tools = Some(
            serde_json::from_value(json!([{"type":"mcp", "server_label":"fixture", "require_approval":"never"}]))
                .unwrap(),
        );
        assert_failed(fixture.run(initial, None).await).await;
        assert_eq!(
            tool.calls.load(Ordering::SeqCst),
            1,
            "remote tool effects are not rolled back"
        );
        assert_eq!(fixture.requests().await.len(), 2);
        assert_eq!(fixture.row_count().await, 0);
        fixture.stop().await;
        tool.stop().await;
    }
}
