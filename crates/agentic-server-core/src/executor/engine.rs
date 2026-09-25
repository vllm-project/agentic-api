//! Stateful conversation executor.
//!
//! Exposes each step of the conversation pipeline as a public function so consumers
//! can compose them directly (e.g. as Praxis filters). [`ExecuteRequest`] is the
//! primary entry point; [`execute`] is a convenience shim for callers that don't
//! need per-request configuration.

mod agent_turn;
mod execute;
mod multi_agent;
mod streaming;
mod usage;

pub use execute::{ExecuteRequest, execute};
#[cfg(test)]
use streaming::panicked_stream_chunks;
use usage::accumulate_usage;

use std::num::NonZeroUsize;

#[cfg(test)]
use either::Either;
#[cfg(test)]
use tokio::sync::mpsc;

use super::compaction::compact_items;
use super::gateway::{
    compaction_event_plans, emit_gateway_completed_events, emit_gateway_start_events, emit_response_start_events,
};
#[cfg(test)]
use super::gateway_accumulator::{GatewayStreamAccumulator, STREAM_EVENT_BUFFER, StreamEvent};
use crate::executor::error::ExecutorResult;
#[cfg(test)]
use crate::executor::inference::DONE_MARKER;
use crate::executor::persist::persist_if_needed;
use crate::executor::pipeline::AgentPipeline;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::response_budget::ExecutorResponseBudget;
#[cfg(test)]
use crate::executor::response_budget::MAX_EXECUTOR_RESPONSE_BYTES;
#[cfg(test)]
use crate::executor::upstream::agent_pipeline;
use crate::executor::upstream::agent_pipeline_with_limits;
use crate::tool::{ToolSearchMetadata, ToolSearchState};
use crate::types::io::{InputItem, OutputItem, ResponseUsage, ResponsesInput};
#[cfg(test)]
use crate::types::request_response::RequestPayload;
use crate::types::request_response::{IncompleteDetails, ResponsePayload};
use crate::utils::common::utcnow_str;
#[cfg(test)]
use agent_turn::build_tool_registry;
use agent_turn::{AgentTurn, RoundDecision, RoundResult};
use multi_agent::MultiAgentRun;

pub use crate::executor::inference::BoxStream;

const MAX_GATEWAY_TOOL_ROUNDS: NonZeroUsize = NonZeroUsize::new(10).unwrap();

async fn run_until_gateway_tools_complete(
    agent: &mut AgentPipeline,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    stream_upstream: bool,
) -> ExecutorResult<(ResponsePayload, Option<ToolSearchMetadata>)> {
    if agent.request.original_request.input.has_compaction_trigger() {
        let tool_search_metadata = agent.take_tool_search_metadata();
        let payload = run_compaction_trigger(&mut agent.request, exec_ctx, auth).await?;
        if let (_, Some((stream_accumulator, stream_sender))) = agent.parts_mut() {
            emit_response_start_events(&payload, stream_accumulator, stream_sender).await?;
            let event_plans = compaction_event_plans(&payload.output, 0);
            emit_gateway_start_events(&event_plans, stream_accumulator, stream_sender).await?;
            emit_gateway_completed_events(&payload.output, &event_plans, stream_accumulator, stream_sender).await?;
        }
        return Ok((payload, tool_search_metadata));
    }
    EngineOrchestration::new(agent, exec_ctx)
        .await?
        .run(auth, stream_upstream)
        .await
}

/// Response-level orchestration. Owns the shared retained-byte budget and
/// response assembly, and advances per-agent execution through typed outcomes.
/// Owns either one agent turn or the tree coordinator. Both use the same
/// pipeline ingestion, delivery, and outer persistence boundary.
enum EngineOrchestration<'a> {
    Single(Box<SingleAgentRun<'a>>),
    Multi {
        coordinator: Box<MultiAgentRun>,
        pipeline: &'a mut AgentPipeline,
        exec_ctx: &'a ExecutionContext,
    },
}

impl<'a> EngineOrchestration<'a> {
    async fn new(agent: &'a mut AgentPipeline, exec_ctx: &'a ExecutionContext) -> ExecutorResult<Self> {
        if agent
            .request
            .enriched_request
            .multi_agent
            .as_ref()
            .is_some_and(|config| config.enabled)
        {
            let coordinator = Box::new(MultiAgentRun::new(agent, exec_ctx).await?);
            Ok(Self::Multi {
                coordinator,
                pipeline: agent,
                exec_ctx,
            })
        } else {
            Ok(Self::Single(Box::new(SingleAgentRun::new(agent, exec_ctx).await?)))
        }
    }

    async fn run(
        self,
        auth: Option<&str>,
        stream_upstream: bool,
    ) -> ExecutorResult<(ResponsePayload, Option<ToolSearchMetadata>)> {
        match self {
            Self::Single(run) => run.run(auth, stream_upstream).await,
            Self::Multi {
                coordinator,
                pipeline,
                exec_ctx,
            } => coordinator
                .run(pipeline, exec_ctx, auth)
                .await
                .map(|payload| (payload, None)),
        }
    }
}

struct SingleAgentRun<'a> {
    root: AgentTurn<'a>,
    response_budget: ExecutorResponseBudget,
    output: Vec<OutputItem>,
    usage: Option<ResponseUsage>,
}

impl<'a> SingleAgentRun<'a> {
    async fn new(agent: &'a mut AgentPipeline, exec_ctx: &'a ExecutionContext) -> ExecutorResult<Self> {
        let response_budget = ExecutorResponseBudget::with_limit(exec_ctx.responses_config.max_retained_bytes);
        let root = AgentTurn::new(agent, exec_ctx, &response_budget, MAX_GATEWAY_TOOL_ROUNDS).await?;
        let output = root.discovery_output();
        Ok(Self {
            root,
            response_budget,
            output,
            usage: None,
        })
    }

    async fn run(
        mut self,
        auth: Option<&str>,
        stream_upstream: bool,
    ) -> ExecutorResult<(ResponsePayload, Option<ToolSearchMetadata>)> {
        loop {
            let RoundResult { mut payload, decision } = self
                .root
                .run_round(self.output.len(), auth, stream_upstream, &self.response_budget)
                .await?;
            accumulate_usage(&mut self.usage, payload.usage.take());
            self.output.append(&mut payload.output);
            match decision {
                RoundDecision::Continue => continue,
                RoundDecision::Incomplete(reason) => {
                    "incomplete".clone_into(&mut payload.status);
                    payload.incomplete_details = Some(IncompleteDetails { reason: Some(reason) });
                }
                RoundDecision::Done | RoundDecision::RequiresClientAction | RoundDecision::UpstreamTerminal => {}
            }
            finalize_loop(&mut payload, self.output, self.usage, &self.root.pipeline.request);
            return Ok((payload, self.root.pipeline.take_tool_search_metadata()));
        }
    }
}

/// Codex CLI remote-compaction V2: the client appends a `compaction_trigger`
/// item to the input and expects the server to run its own summarization turn
/// and stream back exactly one `compaction` output item plus `response.completed`.
/// The trigger never reaches the upstream model; the summary inference is a
/// normal blocking call against the same backend as standalone compaction.
async fn run_compaction_trigger(
    ctx: &mut RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<ResponsePayload> {
    let model = ctx.enriched_request.model.clone();
    let instructions = ctx.enriched_request.instructions.clone();
    let input = std::mem::replace(&mut ctx.enriched_request.input, ResponsesInput::Items(Vec::new()));
    let (mut compacted, usage) = compact_items(&ctx.enriched_request, input, exec_ctx, auth).await?;
    let Some(InputItem::Compaction(compaction)) = compacted.pop() else {
        unreachable!("compact_items always appends a compaction item");
    };
    ctx.new_input_items = compacted;
    if let Some(continuation) = &mut ctx.continuation {
        continuation.mark_history_replaced();
    }
    let mut payload = ResponsePayload {
        id: ctx.response_id.clone(),
        object: "response".to_owned(),
        created_at: utcnow_str(),
        model,
        status: "completed".to_owned(),
        output: vec![OutputItem::Compaction(compaction)],
        usage: Some(usage),
        incomplete_details: None,
        error: None,
        previous_response_id: ctx.original_request.previous_response_id.clone(),
        conversation_id: ctx.conversation_id.clone(),
        instructions,
        tools: None,
        tool_choice: None,
    };
    ctx.inject_ids(&mut payload);
    Ok(payload)
}

/// Move accumulated output/usage onto the terminating round's payload and
/// inject the response/conversation IDs. The payload's `model`/`created_at`/
/// `status` from the latest inference turn are preserved.
fn finalize_loop(
    payload: &mut ResponsePayload,
    combined_output: Vec<crate::types::io::OutputItem>,
    combined_usage: Option<ResponseUsage>,
    ctx: &RequestContext,
) {
    payload.output = combined_output;
    payload.usage = combined_usage;
    ctx.inject_ids(payload);
}

async fn run_blocking(
    ctx: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    max_stream_event_bytes: usize,
) -> ExecutorResult<ResponsePayload> {
    let mut agent = agent_pipeline_with_limits(ctx, tool_search_state, None, max_stream_event_bytes);
    let (payload, tool_search_metadata) =
        Box::pin(run_until_gateway_tools_complete(&mut agent, exec_ctx, auth, false)).await?;
    let (ctx, _) = agent.into_parts();

    let ch = exec_ctx.conv_handler.clone();
    let rh = exec_ctx.resp_handler.clone();
    persist_if_needed(payload.clone(), ctx, tool_search_metadata, ch, rh).await?;

    Ok(payload)
}

/// Create a new conversation and return its data.
///
/// Exposes the conversation-creation step as a standalone function so callers
/// (e.g. `agentic-server`, Praxis filters, or tests) can pre-create a
/// conversation before submitting response turns.
///
/// # Errors
/// Returns [`crate::executor::error::ExecutorError`] if the conversation store is unavailable.
pub async fn create_conversation(exec_ctx: &ExecutionContext) -> ExecutorResult<crate::ConversationData> {
    exec_ctx.conv_handler.create().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::storage::{ConversationStore, InOutItem, ResponseStore, create_pool_with_schema};
    use crate::tool::{GatewayExecutorRegistration, McpDiscoveredHandler, McpHandler};
    use crate::types::tools::McpDiscoveredToolParam;
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn usage_accumulation_preserves_missing_and_explicit_zero_usage() {
        let mut total = None;
        accumulate_usage(&mut total, None);
        assert!(total.is_none());

        accumulate_usage(&mut total, Some(ResponseUsage::default()));
        assert_eq!(total.as_ref().map(|usage| usage.total_tokens), Some(0));

        let first = ResponseUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            ..Default::default()
        };
        accumulate_usage(&mut total, Some(first));
        accumulate_usage(&mut total, None);
        assert_eq!(
            serde_json::to_value(total).unwrap(),
            serde_json::to_value(first).unwrap()
        );

        let second = ResponseUsage {
            input_tokens: 6,
            output_tokens: 3,
            total_tokens: 9,
            ..Default::default()
        };
        accumulate_usage(&mut total, Some(second));
        let total = total.unwrap();
        assert_eq!(total.input_tokens, 16);
        assert_eq!(total.output_tokens, 7);
        assert_eq!(total.total_tokens, 23);
    }

    fn summary_upstream_response() -> serde_json::Value {
        serde_json::json!({
            "id": "resp_upstream",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "msg_upstream",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": "durable summary",
                    "annotations": []
                }]
            }],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 3,
                "total_tokens": 15
            },
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
    }

    async fn trigger_execution_context(
        captured: Arc<Mutex<Option<serde_json::Value>>>,
    ) -> (ExecutionContext, tokio::task::JoinHandle<()>) {
        let captured_for_route = Arc::clone(&captured);
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |body: axum::body::Bytes| async move {
                let value =
                    serde_json::from_slice::<serde_json::Value>(&body).expect("captured upstream body must be JSON");
                *captured_for_route.lock().await = Some(value);
                axum::Json(summary_upstream_response())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock inference server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            format!("http://{address}"),
        );
        (exec_ctx, server)
    }

    async fn streaming_execution_context() -> (ExecutionContext, tokio::task::JoinHandle<()>) {
        const UPSTREAM_SSE: &str = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_upstream\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_upstream\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_upstream\",\"status\":\"completed\",\"usage\":null}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = axum::Router::new()
            .route(
                "/v1/responses",
                axum::routing::post(|_body: axum::body::Bytes| async {
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], UPSTREAM_SSE)
                }),
            )
            // Read the oversized test request before replying, allowing JSON framing
            // above the response budget without relying on an early HTTP response.
            .layer(axum::extract::DefaultBodyLimit::max(
                MAX_EXECUTOR_RESPONSE_BYTES + 64 * 1024,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind streaming mock inference server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            format!("http://{address}"),
        );
        (exec_ctx, server)
    }

    #[tokio::test]
    async fn mcp_discovery_uses_the_request_wide_response_budget() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": "test input",
            "tools": [{"type": "mcp", "server_label": "large-server"}]
        }))
        .expect("valid request");
        let request = RequestContext {
            multi_agent_tree: None,
            original_request: payload.clone(),
            enriched_request: payload,
            new_input_items: Vec::new(),
            response_id: "resp_test".to_owned(),
            conversation_id: None,
            conversation_version: None,
            continuation: None,
        };
        let mut exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            "http://127.0.0.1:1".to_owned(),
        );
        exec_ctx.gateway_executors.insert(GatewayExecutorRegistration::Mcp {
            server_label: "large-server".to_owned(),
            handlers: vec![McpDiscoveredHandler {
                param: McpDiscoveredToolParam {
                    server_label: "large-server".to_owned(),
                    tool_name: "large-tool".to_owned(),
                    internal_name: "mcp__large_server__large_tool".to_owned(),
                    tool: serde_json::from_value(serde_json::json!({
                        "name": "large-tool",
                        "description": "x".repeat(1_024),
                        "inputSchema": {"type": "object"}
                    }))
                    .expect("valid MCP tool"),
                },
                handler: Arc::new(McpHandler::discovered_tool_spec_only()),
            }],
        });
        let response_budget = ExecutorResponseBudget::new();
        response_budget
            .consume(MAX_EXECUTOR_RESPONSE_BYTES - 512)
            .expect("reserve most of the response budget");

        let mut request = agent_pipeline(request, None, None);
        let error = build_tool_registry(&mut request, &exec_ctx, &response_budget)
            .await
            .expect_err("MCP discovery must share the request-wide response budget");

        assert!(matches!(
            error,
            crate::executor::error::ExecutorError::ResourceLimitExceeded {
                limit: crate::executor::error::ResourceLimit::ResponseBudget,
                ..
            } | crate::executor::error::ExecutorError::StreamError(_)
        ));
    }

    #[tokio::test]
    async fn each_mcp_discovery_acquires_and_releases_one_shared_materialization_permit() {
        let mcp_payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": "test input",
            "tools": [
                {"type": "mcp", "server_label": "counter"},
                {"type": "mcp", "server_label": "search"}
            ]
        }))
        .expect("valid MCP request");
        let mcp_request = RequestContext {
            multi_agent_tree: None,
            original_request: mcp_payload.clone(),
            enriched_request: mcp_payload,
            new_input_items: Vec::new(),
            response_id: "resp_mcp".to_owned(),
            conversation_id: None,
            conversation_version: None,
            continuation: None,
        };
        let plain_payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": "test input"
        }))
        .expect("valid request without tools");
        let plain_request = RequestContext {
            multi_agent_tree: None,
            original_request: plain_payload.clone(),
            enriched_request: plain_payload,
            new_input_items: Vec::new(),
            response_id: "resp_plain".to_owned(),
            conversation_id: None,
            conversation_version: None,
            continuation: None,
        };
        let mut exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            "http://127.0.0.1:1".to_owned(),
        );
        exec_ctx.gateway_executors.insert(GatewayExecutorRegistration::Mcp {
            server_label: "counter".to_owned(),
            handlers: vec![McpDiscoveredHandler {
                param: McpDiscoveredToolParam {
                    server_label: "counter".to_owned(),
                    tool_name: "read".to_owned(),
                    internal_name: "mcp__counter__read".to_owned(),
                    tool: serde_json::from_value(serde_json::json!({
                        "name": "read",
                        "inputSchema": {"type": "object"}
                    }))
                    .expect("valid MCP tool"),
                },
                handler: Arc::new(McpHandler::discovered_tool_spec_only()),
            }],
        });
        exec_ctx.gateway_executors.insert(GatewayExecutorRegistration::Mcp {
            server_label: "search".to_owned(),
            handlers: vec![McpDiscoveredHandler {
                param: McpDiscoveredToolParam {
                    server_label: "search".to_owned(),
                    tool_name: "query".to_owned(),
                    internal_name: "mcp__search__query".to_owned(),
                    tool: serde_json::from_value(serde_json::json!({
                        "name": "query",
                        "inputSchema": {"type": "object"}
                    }))
                    .expect("valid MCP tool"),
                },
                handler: Arc::new(McpHandler::discovered_tool_spec_only()),
            }],
        });
        let mut held_permits = Vec::new();
        for _ in 0..crate::executor::gateway::MAX_CONCURRENT_MATERIALIZATIONS {
            held_permits.push(exec_ctx.gateway_scheduler_policy.acquire_materialization_permit().await);
        }

        let plain_budget = ExecutorResponseBudget::new();
        let mut plain_request = agent_pipeline(plain_request, None, None);
        let mut plain_build = Box::pin(build_tool_registry(&mut plain_request, &exec_ctx, &plain_budget));
        assert!(matches!(
            futures::poll!(plain_build.as_mut()),
            std::task::Poll::Ready(Ok(_))
        ));
        drop(plain_build);

        let mcp_budget = ExecutorResponseBudget::new();
        let mut mcp_request = agent_pipeline(mcp_request, None, None);
        let mut mcp_build = Box::pin(build_tool_registry(&mut mcp_request, &exec_ctx, &mcp_budget));
        assert!(futures::poll!(mcp_build.as_mut()).is_pending());

        drop(held_permits.pop());
        mcp_build
            .await
            .expect("sequential MCP discoveries should reuse the released shared permit");
    }

    #[tokio::test]
    async fn compaction_trigger_returns_single_compaction_item_without_upstream_trigger() {
        let captured = Arc::new(Mutex::new(None));
        let (exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": false,
            "store": false,
            "input": [
                {"role": "user", "content": "remember banana"},
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid trigger request");
        let Either::Left(response) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("trigger request succeeds")
        else {
            panic!("non-streaming trigger request must return a payload");
        };

        assert_eq!(response.status, "completed");
        assert_eq!(response.output.len(), 1);
        let OutputItem::Compaction(item) = &response.output[0] else {
            panic!("expected exactly one compaction output item");
        };
        assert_eq!(item.encrypted_content, "durable summary");
        assert!(item.id.as_deref().is_some_and(|id| id.starts_with("cmp_")));
        assert_eq!(response.usage.as_ref().map(|usage| usage.total_tokens), Some(15));

        let upstream = captured.lock().await.take().expect("summary inference ran");
        assert!(
            !upstream.to_string().contains("compaction_trigger"),
            "trigger must never reach the upstream model"
        );
        assert!(upstream.to_string().contains("CONTEXT CHECKPOINT COMPACTION"));
        server.abort();
    }

    #[tokio::test]
    async fn compaction_trigger_persists_checkpoint_only_as_output() {
        let captured = Arc::new(Mutex::new(None));
        let (mut exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        exec_ctx.resp_handler = ResponseHandler::new(response_store.clone());

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "store": true,
            "input": [
                {"role": "user", "content": "remember banana"},
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid trigger request");
        let Either::Left(response) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("trigger request succeeds")
        else {
            panic!("non-streaming trigger request must return a payload");
        };

        let history = response_store
            .rehydrate(&response.id)
            .await
            .expect("compaction trigger response rehydrates");
        assert_eq!(history.len(), 2);
        assert!(matches!(history[0], InOutItem::Input(InputItem::Message(_))));
        assert!(matches!(history[1], InOutItem::Output(OutputItem::Compaction(_))));

        let model_input = ResponsesInput::Items(InOutItem::into_input_items(history));
        let serialized = serde_json::to_value(model_input.model_input()).expect("model input serializes");
        assert_eq!(serialized.as_array().map(Vec::len), Some(2));
        assert_eq!(serialized[0]["content"], "remember banana");
        assert_eq!(serialized[1]["role"], "assistant");
        assert_eq!(serialized[1]["content"][0]["text"], "durable summary");
        server.abort();
    }

    async fn streaming_response(
        payload: RequestPayload,
        exec_ctx: Arc<ExecutionContext>,
    ) -> (ResponsePayload, Vec<serde_json::Value>) {
        match ExecuteRequest::new(payload, exec_ctx)
            .run()
            .await
            .expect("request succeeds")
        {
            Either::Left(_) => panic!("streaming request must return a stream"),
            Either::Right(stream) => {
                let events = stream
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .flat_map(|chunk| {
                        chunk
                            .lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                let response = events
                    .iter()
                    .find_map(|event| {
                        (event["type"] == "response.completed")
                            .then(|| serde_json::from_value(event["response"].clone()).ok())
                            .flatten()
                    })
                    .expect("stream contains a completed response");
                (response, events)
            }
        }
    }

    #[tokio::test]
    async fn oversized_terminal_response_is_not_persisted() {
        let (mut exec_ctx, server) = streaming_execution_context().await;
        exec_ctx.responses_config.max_retained_bytes = 512 * 1024;
        exec_ctx.responses_config.max_stream_event_bytes = 512 * 1024;
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        exec_ctx.resp_handler = ResponseHandler::new(response_store.clone());

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": true,
            "store": true,
            "input": "short input",
            "instructions": "x".repeat(MAX_EXECUTOR_RESPONSE_BYTES),
        }))
        .expect("valid oversized request");
        let Either::Right(stream) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("request setup succeeds")
        else {
            panic!("streaming request must return a stream");
        };

        let events = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flat_map(|chunk| {
                chunk
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let response_id = events
            .iter()
            .find(|event| event["type"] == "response.created")
            .and_then(|event| event["response"]["id"].as_str())
            .expect("created event has response ID");

        assert!(events.iter().all(|event| event["type"] != "response.completed"));
        assert!(events.iter().any(|event| {
            event["type"] == "error"
                && event["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("stream event exceeded"))
        }));
        assert!(
            response_store
                .get(response_id)
                .await
                .expect_err("oversized terminal response must not be persisted")
                .is_not_found()
        );
        server.abort();
    }

    fn mcp_list_tools_lifecycle_event_count(events: &[serde_json::Value]) -> usize {
        events
            .iter()
            .filter(|event| {
                event["type"]
                    .as_str()
                    .is_some_and(|event_type| event_type.starts_with("response.mcp_list_tools."))
                    || event["item"]["type"] == "mcp_list_tools"
            })
            .count()
    }

    #[tokio::test]
    async fn streaming_previous_response_continuation_emits_mcp_list_tools_only_once() {
        let (mut exec_ctx, server) = streaming_execution_context().await;
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        exec_ctx.resp_handler = ResponseHandler::new(ResponseStore::new(pool));
        exec_ctx.gateway_executors.insert(GatewayExecutorRegistration::Mcp {
            server_label: "counter".to_owned(),
            handlers: vec![McpDiscoveredHandler {
                param: McpDiscoveredToolParam {
                    server_label: "counter".to_owned(),
                    tool_name: "read".to_owned(),
                    internal_name: "mcp__counter__read".to_owned(),
                    tool: serde_json::from_value(serde_json::json!({
                        "name": "read",
                        "description": "Read the counter",
                        "inputSchema": {"type": "object"}
                    }))
                    .expect("valid MCP tool"),
                },
                handler: Arc::new(McpHandler::discovered_tool_spec_only()),
            }],
        });
        let exec_ctx = Arc::new(exec_ctx);

        let first_request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": true,
            "store": true,
            "input": "first turn",
            "tools": [{
                "type": "mcp",
                "server_label": "counter",
                "allowed_tools": ["read"],
                "require_approval": "never"
            }]
        }))
        .expect("valid first request");
        let (first_response, first_events) = streaming_response(first_request, Arc::clone(&exec_ctx)).await;
        assert_eq!(
            first_response
                .output
                .iter()
                .filter(|item| matches!(item, OutputItem::McpListTools(_)))
                .count(),
            1
        );
        assert_eq!(mcp_list_tools_lifecycle_event_count(&first_events), 4);

        let second_request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": true,
            "store": true,
            "input": "second turn",
            "previous_response_id": first_response.id,
            "tools": [{
                "type": "mcp",
                "server_label": "counter",
                "allowed_tools": ["read"],
                "require_approval": "never"
            }]
        }))
        .expect("valid continuation request");
        let (second_response, second_events) = streaming_response(second_request, exec_ctx).await;
        assert!(
            second_response
                .output
                .iter()
                .all(|item| !matches!(item, OutputItem::McpListTools(_)))
        );
        assert_eq!(mcp_list_tools_lifecycle_event_count(&second_events), 0);

        server.abort();
    }

    #[tokio::test]
    async fn compaction_trigger_streams_one_compaction_item_then_completed() {
        let captured = Arc::new(Mutex::new(None));
        let (exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": true,
            "store": false,
            "input": [
                {"role": "user", "content": "remember banana"},
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid trigger request");
        let Either::Right(stream) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("trigger request succeeds")
        else {
            panic!("streaming trigger request must return a stream");
        };

        let chunks: Vec<String> = stream.collect().await;
        let mut event_types = Vec::new();
        let mut compaction_done_count = 0;
        for chunk in chunks {
            let body = chunk.strip_suffix("\n\n").expect("SSE frame terminator");
            let data = body
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("SSE data line");
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue; // terminal [DONE] marker
            };
            let event_type = event["type"].as_str().expect("event type");
            event_types.push(event_type.to_owned());
            if matches!(event_type, "response.created" | "response.in_progress") {
                assert_eq!(event["response"]["output"], serde_json::json!([]));
                assert!(event["response"]["usage"].is_null());
            }
            if event_type == "response.output_item.done" && event["item"]["type"] == "compaction" {
                compaction_done_count += 1;
                assert_eq!(event["item"]["encrypted_content"], "durable summary");
            }
        }

        assert_eq!(
            event_types,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(compaction_done_count, 1);
        assert!(
            !captured
                .lock()
                .await
                .take()
                .expect("summary inference ran")
                .to_string()
                .contains("compaction_trigger")
        );
        server.abort();
    }

    #[tokio::test]
    async fn stream_task_panic_after_event_uses_next_sequence_number_for_error() {
        let accumulator = GatewayStreamAccumulator::new();
        let (event_tx, mut event_rx) = mpsc::channel(STREAM_EVENT_BUFFER);
        let task = tokio::spawn(async move {
            let mut accumulator = accumulator;
            let event = accumulator
                .process_sse_line(r#"data: {"type":"response.created"}"#, 0)
                .expect("event should be emitted");
            event_tx
                .try_send(StreamEvent {
                    content: "event".to_owned(),
                    sequence_number: event.sequence_number().expect("event should be numbered"),
                })
                .expect("test receiver should remain open");
            panic!("test task panic");
        });

        let error = task.await.expect_err("task should panic");
        let mut next_sequence_number = 0;
        let chunks = panicked_stream_chunks(&error, &mut event_rx, &mut next_sequence_number);
        let mut error_lines = chunks[1].lines();
        assert_eq!(error_lines.next(), Some("event: error"));
        let error_data = error_lines
            .next()
            .and_then(|line| line.strip_prefix("data: "))
            .expect("SSE data");
        assert!(error_lines.all(str::is_empty), "unexpected SSE frame content");
        let error_event: serde_json::Value =
            serde_json::from_str(error_data).expect("error chunk should be valid JSON");

        assert_eq!(chunks[0], "event");
        assert_eq!(error_event["type"], "error");
        assert_eq!(error_event["sequence_number"], 1);
        assert_eq!(chunks[2], DONE_MARKER);
    }
}
