//! Exercise scheduling through both complete Messages loops, including the
//! next upstream request. Handcrafted responses deliberately control fan-out;
//! recorded protocol compatibility is covered by the Messages cassette tests.

use std::fmt::Write as _;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::response::IntoResponse as _;
use axum::{Json, Router, routing::post};
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::executor::gateway::{GatewaySchedulerPolicy, MAX_CONCURRENT_MATERIALIZATIONS};
use crate::executor::{
    ConversationHandler, ExecutionContext, MessagesRequestContext, MessagesUpstream, ResponseHandler,
    run_messages_loop, run_messages_stream,
};
use crate::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use crate::tool::{GatewayExecutor, ToolError, ToolHandler, ToolOutput, ToolRegistry, ToolType, registry_tools};
use crate::types::io::FunctionTool;
use crate::types::messages::GatewayToolMap;
use crate::types::tools::WebSearchToolParam;

mod lifecycle;

struct Started {
    id: String,
    finish: oneshot::Sender<Result<ToolOutput, ToolError>>,
}

impl Started {
    fn complete(self) {
        let output = format!("result for {}", self.id);
        self.finish.send(Ok(ToolOutput::success(self.id, output))).unwrap();
    }
}

struct Probe {
    started: mpsc::Sender<Started>,
    active: Arc<AtomicUsize>,
    parallel: bool,
}

struct ActiveCall(Arc<AtomicUsize>);

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ToolHandler for Probe {
    type ToolParams = WebSearchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }

    fn validate(&self, _: &Self::ToolParams) -> Result<(), ToolError> {
        Ok(())
    }

    fn normalize(&self, _: &Self::ToolParams) -> Vec<FunctionTool> {
        Vec::new()
    }
}

impl GatewayExecutor for Probe {
    type ExecutionParams = WebSearchToolParam;

    fn supports_parallel_execution(&self) -> bool {
        self.parallel
    }

    fn execute(
        &self,
        call_id: &str,
        _: &str,
        _: &str,
        _: &Self::ExecutionParams,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let id = call_id.to_owned();
        Box::pin(async move {
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveCall(Arc::clone(&self.active));
            let (finish, result) = oneshot::channel();
            self.started.send(Started { id, finish }).await.unwrap();
            result.await.unwrap()
        })
    }
}

struct Harness {
    started: mpsc::Receiver<Started>,
    requests: mpsc::Receiver<Value>,
    active: Arc<AtomicUsize>,
    task: JoinHandle<Value>,
    server: JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
        self.server.abort();
    }
}

fn round_body(names: &[&str]) -> Value {
    json!({
        "type":"message", "id":"msg_test", "role":"assistant", "model":"test",
        "content": names.iter().enumerate().map(|(index, name)| json!({
            "type":"tool_use", "id":format!("call_{index}"), "name":name,
            "input":{"query":format!("query {index}")}
        })).collect::<Vec<_>>(),
        "stop_reason": if names.is_empty() { "end_turn" } else { "tool_use" },
        "stop_sequence":null, "usage":{"input_tokens":10, "output_tokens":5}
    })
}

fn round_sse(message: &Value) -> String {
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    let mut events = vec![json!({"type":"message_start", "message":start})];
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut skeleton = block.clone();
        skeleton["input"] = json!({});
        events.extend([
            json!({"type":"content_block_start", "index":index, "content_block":skeleton}),
            json!({"type":"content_block_delta", "index":index,
                "delta":{"type":"input_json_delta", "partial_json":block["input"].to_string()}}),
            json!({"type":"content_block_stop", "index":index}),
        ]);
    }
    events.extend([
        json!({"type":"message_delta", "delta":{"stop_reason":message["stop_reason"], "stop_sequence":null},
            "usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]);
    let mut sse = String::new();
    for event in events {
        write!(sse, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    sse
}

impl Harness {
    async fn new(streaming: bool, names: &[&str], parallel: bool, policy: GatewaySchedulerPolicy) -> Self {
        let first = round_body(names);
        let last = round_body(&[]);
        let (request_tx, requests) = mpsc::channel(4);
        let rounds = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/messages",
            post(move |Json(request): Json<Value>| {
                let sender = request_tx.clone();
                let body = if rounds.fetch_add(1, Ordering::SeqCst) == 0 {
                    first.clone()
                } else {
                    last.clone()
                };
                async move {
                    sender.send(request).await.unwrap();
                    if streaming {
                        ([(http::header::CONTENT_TYPE, "text/event-stream")], round_sse(&body)).into_response()
                    } else {
                        Json(body).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let mut exec = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(pool)),
            Arc::new(reqwest::Client::new()),
            url.clone(),
        );
        exec.gateway_scheduler_policy = policy;
        exec.messages_gateway_tools = GatewayToolMap::from_env_str("WebSearch=web_search");
        let (started_tx, started) = mpsc::channel(64);
        let active = Arc::new(AtomicUsize::new(0));
        exec = exec.with_gateway_executor(Arc::new(Probe {
            started: started_tx,
            active: Arc::clone(&active),
            parallel,
        }));
        let context = MessagesRequestContext::from_value(json!({
            "model":"test", "max_tokens":128, "stream":streaming,
            "messages":[{"role":"user", "content":"search"}],
            "tools":[{"name":"web_search", "input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let mut declarations = registry_tools(context.tools(), &exec.messages_gateway_tools);
        let registry = ToolRegistry::build_with_handlers(&mut declarations, &mut exec.gateway_executors)
            .await
            .unwrap();
        let upstream = MessagesUpstream::new(&url, None, http::HeaderMap::new());
        let task = tokio::spawn(async move {
            if streaming {
                let response = run_messages_stream(context, Arc::new(registry), Arc::new(exec), upstream)
                    .await
                    .unwrap();
                let text = response.body.collect::<Vec<_>>().await.concat();
                assert_eq!(text.matches("event: message_start\n").count(), 1);
                assert_eq!(text.matches("event: message_stop\n").count(), 1);
                assert!(!text.contains("event: error"), "{text}");
                assert!(!text.contains("tool_use"), "gateway calls stay hidden: {text}");
                Value::Null
            } else {
                let response = run_messages_loop(context, &registry, &exec, &upstream).await.unwrap();
                assert_eq!(response.body["stop_reason"], "end_turn");
                assert_eq!(response.body["content"], json!([]));
                response.body
            }
        });
        Self {
            started,
            requests,
            active,
            task,
            server,
        }
    }

    async fn next(&mut self, expected: &str) -> Started {
        let started = tokio::time::timeout(Duration::from_secs(5), self.started.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(started.id, expected);
        started
    }

    async fn no_more_started(&mut self) {
        tokio::task::yield_now().await;
        assert!(matches!(self.started.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
    }

    async fn finish(&mut self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        self.requests.recv().await.unwrap();
        let next = self.requests.recv().await.unwrap();
        let results = next["messages"][2]["content"].as_array().unwrap();
        assert_eq!(results.len(), count);
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result["tool_use_id"], format!("call_{index}"));
            assert_eq!(result["content"], format!("result for call_{index}"));
            assert_eq!(result["is_error"], false);
        }
        assert!(self.requests.try_recv().is_err(), "only one continuation round");
    }
}

fn policy(limit: usize) -> GatewaySchedulerPolicy {
    GatewaySchedulerPolicy::new(NonZeroUsize::new(limit).unwrap())
}

#[tokio::test]
async fn messages_scheduling_execution_windows_are_per_round() {
    for streaming in [false, true] {
        let policy = policy(1);
        let mut first = Harness::new(streaming, &["web_search"; 2], true, policy.clone()).await;
        let mut second = Harness::new(streaming, &["web_search"; 2], true, policy).await;
        for index in 0..2 {
            // A busy round must not consume another request's execution window.
            let a = first.next(&format!("call_{index}")).await;
            let b = second.next(&format!("call_{index}")).await;
            first.no_more_started().await;
            second.no_more_started().await;
            a.complete();
            b.complete();
        }
        first.finish(2).await;
        second.finish(2).await;
    }
}

#[tokio::test]
async fn messages_scheduling_bounds_and_refills_both_loops_in_model_order() {
    for streaming in [false, true] {
        // Exercise both join_all implementations (small and large fan-out),
        // keeping the first call blocked while every later slot is refilled.
        for count in [4, 40] {
            let mut run = Harness::new(streaming, &vec!["web_search"; count], true, policy(2)).await;
            let first = run.next("call_0").await;
            for index in 1..count {
                let later = run.next(&format!("call_{index}")).await;
                run.no_more_started().await;
                assert_eq!(run.active.load(Ordering::SeqCst), 2);
                later.complete();
            }
            first.complete();
            run.finish(count).await;
        }
    }
}

#[tokio::test]
async fn messages_scheduling_serializes_aliases_of_exclusive_handler() {
    for streaming in [false, true] {
        let mut run = Harness::new(streaming, &["web_search", "WebSearch", "web_search"], false, policy(3)).await;
        for index in 0..3 {
            let call = run.next(&format!("call_{index}")).await;
            run.no_more_started().await;
            assert_eq!(run.active.load(Ordering::SeqCst), 1);
            call.complete();
        }
        run.finish(3).await;
    }
}

#[tokio::test]
async fn messages_scheduling_shares_materialization_permits_and_cancels_waiters() {
    for streaming in [false, true] {
        let policy = policy(3);
        let mut held = Vec::new();
        for _ in 1..MAX_CONCURRENT_MATERIALIZATIONS {
            held.push(policy.acquire_materialization_permit().await);
        }
        let mut run = Harness::new(streaming, &["web_search"; 4], true, policy.clone()).await;
        let first = run.next("call_0").await;
        run.no_more_started().await;
        drop(held.pop());
        let second = run.next("call_1").await;
        run.no_more_started().await;
        run.task.abort();
        assert!((&mut run.task).await.unwrap_err().is_cancelled());
        assert_eq!(run.active.load(Ordering::SeqCst), 0);
        assert!(first.finish.is_closed());
        assert!(second.finish.is_closed());
        assert!(run.started.try_recv().is_err(), "queued handlers never started");
        drop(held);
        let mut recovered = Vec::new();
        for _ in 0..MAX_CONCURRENT_MATERIALIZATIONS {
            recovered.push(
                tokio::time::timeout(Duration::from_secs(5), policy.acquire_materialization_permit())
                    .await
                    .unwrap(),
            );
        }
    }
}
