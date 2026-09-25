//! Recorded Responses/tool exchanges exercise stream ownership, not provider qualification.

mod support;

use std::{
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agentic_core::executor::{
    BoxStream, ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler, ResponseSession,
    ResponseSessionGroup, rehydrate_conversation, rehydrate_in_session,
};
use agentic_core::storage::{ConversationStore, DbPool, ResponseStore, create_pool_with_schema};
use agentic_core::tool::{GatewayExecutor, ToolError, ToolHandler, ToolOutput, ToolType, WebSearchHandler};
use agentic_core::types::{FunctionTool, InputItem, RequestPayload, ResponsesInput, WebSearchToolParam};
use either::Either;
use futures::StreamExt;
use serde_json::json;

const REASONING: &str = "reasoning/responses/reasoning-single-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml";
const TOOL: &str = "codex/codex-vllm-multi-round-web-search-gpt-oss-20b-streaming.yaml";

fn recording(path: &str) -> support::Cassette {
    support::load_cassette(&format!("{}/tests/cassettes/{path}", env!("CARGO_MANIFEST_DIR")))
}

fn request(turn: &support::Turn, store: bool) -> RequestPayload {
    serde_json::from_value(json!({
        "model":turn.request.body.model, "input":turn.request.body.input,
        "store":store, "stream":true, "reasoning":turn.request.body.reasoning,
        "max_output_tokens":turn.request.body.max_output_tokens,
    }))
    .unwrap()
}

fn session() -> ResponseSession {
    ResponseSession::new(NonZeroUsize::new(128).unwrap(), NonZeroUsize::new(1_000_000).unwrap())
}

struct Fixture {
    context: Arc<ExecutionContext>,
    pool: Arc<DbPool>,
    server: support::MockServer,
}

impl Fixture {
    async fn new(turn: &support::Turn) -> Self {
        Self::with_turns(&[turn]).await
    }

    async fn with_turns(turns: &[&support::Turn]) -> Self {
        let server = support::MockServer::start_deque(
            turns
                .iter()
                .map(|turn| support::MockResponse::from_turn(turn))
                .collect(),
        )
        .await;
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let context = Arc::new(ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
            Arc::new(reqwest::Client::new()),
            server.url().into(),
        ));
        Self { context, pool, server }
    }

    async fn stream(&self, request: RequestPayload, session: &ResponseSession) -> BoxStream {
        let Either::Right(stream) = ExecuteRequest::new(request, Arc::clone(&self.context))
            .with_session(session)
            .unwrap()
            .run()
            .await
            .unwrap()
        else {
            panic!("stream")
        };
        stream
    }

    fn assert_idle_now(&self, session: &ResponseSession) {
        // No await or wait_until_idle: disposal must already have released the lease.
        let next = ExecuteRequest::new(
            support::make_request("probe", false, true, None, None),
            Arc::clone(&self.context),
        )
        .with_session(session)
        .expect("stream drop must synchronously release the execution lease");
        drop(next);
    }

    async fn assert_no_rows(&self) {
        let counts: (i64, i64) =
            sqlx::query_as("SELECT (SELECT COUNT(*) FROM responses), (SELECT COUNT(*) FROM items)")
                .fetch_one(self.pool.as_ref())
                .await
                .unwrap();
        assert_eq!(counts, (0, 0));
    }
}

#[tokio::test]
async fn dropping_an_unpolled_response_releases_its_session_without_inference() {
    let cassette = recording(REASONING);
    let fixture = Fixture::new(&cassette.turns[0]).await;
    let session = session();
    let stream = fixture.stream(request(&cassette.turns[0], true), &session).await;
    drop(stream);
    fixture.assert_idle_now(&session);
    assert!(fixture.server.request_bodies().await.is_empty());
    fixture.assert_no_rows().await;
    fixture.pool.close().await;
}

#[tokio::test]
async fn dropping_a_partially_delivered_recording_releases_its_session_without_persistence() {
    let cassette = recording(REASONING);
    let fixture = Fixture::new(&cassette.turns[0]).await;
    let session = session();
    let mut stream = fixture.stream(request(&cassette.turns[0], true), &session).await;
    assert!(stream.next().await.unwrap().contains("response.created"));
    drop(stream);
    fixture.assert_idle_now(&session);
    fixture.assert_no_rows().await;
    assert_eq!(fixture.server.request_bodies().await.len(), 1);
    fixture.pool.close().await;
}

struct ToolGuard(Arc<AtomicUsize>);

impl Drop for ToolGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct ControlledSearch {
    started: tokio::sync::Notify,
    dropped: Arc<AtomicUsize>,
    panic: bool,
}

impl ToolHandler for ControlledSearch {
    type ToolParams = WebSearchToolParam;
    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }
    fn validate(&self, params: &Self::ToolParams) -> Result<(), ToolError> {
        WebSearchHandler::spec_only().validate(params)
    }
    fn normalize(&self, params: &Self::ToolParams) -> Vec<FunctionTool> {
        WebSearchHandler::spec_only().normalize(params)
    }
}

impl GatewayExecutor for ControlledSearch {
    type ExecutionParams = WebSearchToolParam;
    fn execute(
        &self,
        _call_id: &str,
        _name: &str,
        _arguments: &str,
        _params: &Self::ExecutionParams,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let _guard = ToolGuard(Arc::clone(&self.dropped));
            self.started.notify_one();
            assert!(!self.panic, "private tool panic payload");
            std::future::pending().await
        })
    }
}

async fn tool_fixture(panic: bool) -> (Fixture, Arc<ControlledSearch>, RequestPayload) {
    let cassette = recording(TOOL);
    let mut fixture = Fixture::new(&cassette.turns[0]).await;
    let handler = Arc::new(ControlledSearch {
        started: tokio::sync::Notify::new(),
        dropped: Arc::new(AtomicUsize::new(0)),
        panic,
    });
    fixture.context = Arc::new(
        fixture
            .context
            .as_ref()
            .clone()
            .with_gateway_executor(Arc::clone(&handler)),
    );
    let mut request = request(&cassette.turns[0], true);
    request.tools = Some(serde_json::from_value(json!([{"type":"web_search_preview"}])).unwrap());
    (fixture, handler, request)
}

#[tokio::test]
async fn cancelling_during_recorded_tool_execution_drops_the_tool_and_session_inline() {
    let (fixture, handler, request) = tool_fixture(false).await;
    let session = session();
    let mut stream = fixture.stream(request, &session).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            () = handler.started.notified() => {},
            () = async { while stream.next().await.is_some() {} } => panic!("tool should remain suspended"),
        }
    })
    .await
    .unwrap();
    assert_eq!(handler.dropped.load(Ordering::SeqCst), 0);
    drop(stream);
    assert_eq!(handler.dropped.load(Ordering::SeqCst), 1);
    fixture.assert_idle_now(&session);
    fixture.assert_no_rows().await;
    assert_eq!(
        fixture.server.request_bodies().await.len(),
        1,
        "no late inference round"
    );
    fixture.pool.close().await;
}

#[tokio::test]
async fn producer_panic_drains_numbered_events_then_one_redacted_error_without_storing() {
    let (fixture, handler, request) = tool_fixture(true).await;
    let session = session();
    let stream = fixture.stream(request, &session).await;
    let chunks: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect())
        .await
        .unwrap();
    assert_eq!(handler.dropped.load(Ordering::SeqCst), 1);
    fixture.assert_idle_now(&session);
    let events = support::streamed_sse_events(&chunks);
    assert!(!events.is_empty());
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(
            event["sequence_number"].as_u64().unwrap(),
            u64::try_from(sequence).unwrap()
        );
        assert_ne!(event["type"], "response.completed");
    }
    assert_eq!(events.iter().filter(|event| event["type"] == "error").count(), 1);
    assert_eq!(events.last().unwrap()["error"]["message"], "stream producer panicked");
    assert!(!chunks.join("").contains("private tool panic payload"));
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| chunk.as_str() == "data: [DONE]\n\n")
            .count(),
        1
    );
    fixture.assert_no_rows().await;
    fixture.pool.close().await;
}

#[tokio::test]
async fn terminal_event_still_follows_persistence_and_checkpoint_publication() {
    let cassette = recording(REASONING);
    let fixture = Fixture::new(&cassette.turns[0]).await;
    let session = session();
    let mut stream = fixture.stream(request(&cassette.turns[0], true), &session).await;
    let mut terminal = None;
    while let Some(chunk) = stream.next().await {
        let Some(event) = support::streamed_sse_event(&chunk) else {
            continue;
        };
        if event["type"] == "response.completed" {
            terminal = Some(event["response"]["id"].as_str().unwrap().to_owned());
            break;
        }
    }
    let id = terminal.expect("recorded response completed");
    // Do not poll DONE first. State must already be usable at terminal delivery.
    let followup = support::make_request("continue", false, false, Some(id.clone()), None);
    let stored = rehydrate_conversation(followup.clone(), &fixture.context)
        .await
        .unwrap();
    assert!(
        !fixture
            .context
            .resp_handler
            .rehydrate(&stored)
            .await
            .unwrap()
            .is_empty()
    );
    let checkpoint = rehydrate_in_session(followup, &fixture.context, &session)
        .await
        .unwrap();
    assert!(matches!(checkpoint.enriched_request.input, ResponsesInput::Items(_)));
    drop(checkpoint);
    drop(stream);
    fixture.pool.close().await;
}

#[tokio::test]
async fn cancellation_while_persistence_waits_drops_the_session_and_writes_nothing() {
    let cassette = recording(REASONING);
    let fixture = Fixture::new(&cassette.turns[0]).await;
    assert_eq!(fixture.pool.options().get_max_connections(), 1);
    let session = session();
    let mut stream = fixture.stream(request(&cassette.turns[0], true), &session).await;
    let held = fixture.pool.acquire().await.unwrap();
    let mut output_completed = false;
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(chunk) = stream.next().await {
            if let Some(event) = support::streamed_sse_event(&chunk) {
                assert_ne!(event["type"], "response.completed");
                output_completed |= event["type"] == "response.output_item.done" && event["item"]["type"] == "message";
            }
        }
    })
    .await;
    assert!(result.is_err(), "persistence must wait for its connection");
    assert!(
        output_completed,
        "the complete recorded output was ingested before the storage wait"
    );
    drop(stream);
    fixture.assert_idle_now(&session);
    drop(held);
    fixture.assert_no_rows().await;
    fixture.pool.close().await;
}

#[tokio::test]
async fn dropping_a_streaming_fork_preserves_the_sources_reasoning_checkpoint() {
    let cassette = recording(REASONING);
    let fixture = Fixture::with_turns(&[&cassette.turns[0], &cassette.turns[0]]).await;
    let group = ResponseSessionGroup::new(
        NonZeroUsize::new(2).unwrap(),
        NonZeroUsize::new(128).unwrap(),
        NonZeroUsize::new(1_000_000).unwrap(),
        NonZeroUsize::new(2_000_000).unwrap(),
    );
    let source = group.new_session().unwrap();
    let fork = group.new_session().unwrap();
    let stream = fixture.stream(request(&cassette.turns[0], false), &source).await;
    let response = support::collect_stream(Either::Right(stream)).await;
    let mut followup = support::make_request("fork", false, true, Some(response.id), None);
    let mut cancelled = fixture.stream(followup.clone(), &fork).await;
    assert!(cancelled.next().await.unwrap().contains("response.created"));
    drop(cancelled);
    fixture.assert_idle_now(&fork);
    followup.stream = false;
    let parent = rehydrate_in_session(followup, &fixture.context, &source).await.unwrap();
    let ResponsesInput::Items(items) = &parent.enriched_request.input else {
        panic!("parent history")
    };
    assert!(
        items
            .iter()
            .any(|item| matches!(item, InputItem::Reasoning(reasoning) if reasoning.replay_provenance.is_some()))
    );
    drop(parent);
    assert_eq!(fixture.server.request_bodies().await.len(), 2);
    fixture.assert_no_rows().await;
    fixture.pool.close().await;
}
