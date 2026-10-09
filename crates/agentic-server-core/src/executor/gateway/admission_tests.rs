//! `max_tool_calls` admission through the gateway scheduler and its public lifecycle.

use super::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::executor::gateway_accumulator::StreamEvent;
use crate::executor::relay::{RelayLimits, StreamRelay};
use crate::tool::{
    GatewayExecutor, GatewayExecutors, GatewayToolEventPlan, ToolHandler, ToolType, responses_declarations,
};
use crate::types::io::FunctionTool;
use crate::types::io::output::WebSearchCallStatus;
use crate::types::tools::{ResponsesTool, WebSearchToolParam};

/// Web search that counts executions and exposes the real public lifecycle.
#[derive(Default)]
struct CountingSearch {
    executions: AtomicUsize,
}

impl ToolHandler for CountingSearch {
    type ToolParams = WebSearchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }

    fn validate(&self, _params: &WebSearchToolParam) -> Result<(), ToolError> {
        Ok(())
    }

    fn normalize(&self, _params: &WebSearchToolParam) -> Vec<FunctionTool> {
        Vec::new()
    }
}

impl GatewayExecutor for CountingSearch {
    type ExecutionParams = WebSearchToolParam;

    fn execute(
        &self,
        call_id: &str,
        _tool_name: &str,
        _arguments: &str,
        _params: &WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let call_id = call_id.to_owned();
        Box::pin(async move { Ok(ToolOutput::success(call_id, r#"{"query":"q"}"#)) })
    }

    fn supports_parallel_execution(&self) -> bool {
        true
    }

    fn plan_gateway_events(&self, call: &FunctionToolCall, _params: &WebSearchToolParam) -> GatewayToolEventPlan {
        GatewayToolEventPlan::new(crate::tool::web_search::started_output_item(call))
    }

    fn public_output(
        &self,
        call: &FunctionToolCall,
        output: &ToolOutput,
        status: GatewayCallStatus,
        _params: &WebSearchToolParam,
    ) -> Option<OutputItem> {
        crate::tool::web_search::output_item(call, output, status.into())
    }
}

async fn registry(search: &Arc<CountingSearch>, declarations: serde_json::Value) -> ToolRegistry {
    let tools: Vec<ResponsesTool> = serde_json::from_value(declarations).expect("tool declarations");
    let mut tools = responses_declarations(&tools);
    let mut executors = GatewayExecutors::default();
    executors.insert(Arc::clone(search));
    ToolRegistry::build_with_handlers(&mut tools, &mut executors)
        .await
        .expect("registry builds")
}

fn call(name: &str, call_id: &str) -> OutputItem {
    OutputItem::FunctionCall(FunctionToolCall {
        agent: None,
        id: format!("fc_{call_id}"),
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: r#"{"query":"q"}"#.to_owned(),
        status: crate::types::event::MessageStatus::Completed,
        namespace: None,
    })
}

fn limit(value: u64) -> BuiltInToolCallBudget {
    BuiltInToolCallBudget::new(NonZeroU64::new(value))
}

fn tool_output(result: &GatewayCallResult) -> String {
    let InputItem::FunctionCallOutput(output) = &result.input_item else {
        panic!("every gateway call is answered with a function_call_output");
    };
    serde_json::to_string(output).expect("serialize tool output")
}

async fn plan_and_run(
    items: &[OutputItem],
    registry: &ToolRegistry,
    budget: &mut BuiltInToolCallBudget,
) -> (GatewayScheduler, Vec<GatewayCallResult>) {
    let mut scheduler =
        GatewayScheduler::plan_with_budget(items, registry, 0, GatewaySchedulerPolicy::default(), budget);
    let results = scheduler.execute().await.expect("round executes");
    (scheduler, results)
}

#[tokio::test]
async fn refused_calls_never_execute_and_only_the_first_is_public() {
    let search = Arc::new(CountingSearch::default());
    let registry = registry(&search, serde_json::json!([{"type": "web_search"}])).await;
    let items = ["a", "b", "c", "d"].map(|id| call("web_search", id));
    let mut budget = limit(2);

    let (scheduler, results) = plan_and_run(&items, &registry, &mut budget).await;

    assert_eq!(
        search.executions.load(Ordering::SeqCst),
        2,
        "refused calls must not execute"
    );
    assert!(budget.has_refused());
    for refused in &results[2..] {
        assert!(tool_output(refused).contains("UserError: Reached tool call limit of 2"));
    }
    let Some(OutputItem::WebSearchCall(shown)) = &results[2].public_output else {
        panic!("the first refused web search stays public");
    };
    assert_eq!(shown.status, WebSearchCallStatus::Searching);
    assert!(!results[2].omitted);
    assert!(results[3].omitted && results[3].public_output.is_none());

    let public = public_output_items(&items, &registry, &results).expect("public projection");
    let statuses: Vec<_> = public
        .iter()
        .map(|item| match item {
            OutputItem::WebSearchCall(call) => call.status,
            other => panic!("unexpected public item {other:?}"),
        })
        .collect();
    assert_eq!(
        statuses,
        [
            WebSearchCallStatus::Completed,
            WebSearchCallStatus::Completed,
            WebSearchCallStatus::Searching
        ]
    );
    assert_eq!(scheduler.public_item_index(3), None);
}

#[tokio::test]
async fn one_budget_spans_inference_rounds() {
    let search = Arc::new(CountingSearch::default());
    let registry = registry(&search, serde_json::json!([{"type": "web_search"}])).await;
    let mut budget = limit(2);

    let (_, first) = plan_and_run(&[call("web_search", "a")], &registry, &mut budget).await;
    let (_, second) = plan_and_run(
        &[call("web_search", "b"), call("web_search", "c")],
        &registry,
        &mut budget,
    )
    .await;
    let (_, third) = plan_and_run(&[call("web_search", "d")], &registry, &mut budget).await;

    assert_eq!(search.executions.load(Ordering::SeqCst), 2);
    assert!(
        matches!(first[0].public_output, Some(OutputItem::WebSearchCall(ref c)) if c.status == WebSearchCallStatus::Completed)
    );
    assert!(
        matches!(second[1].public_output, Some(OutputItem::WebSearchCall(ref c)) if c.status == WebSearchCallStatus::Searching)
    );
    assert!(third[0].omitted, "only the first refusal in a response is public");
}

#[tokio::test]
async fn client_function_calls_never_consume_the_budget() {
    let search = Arc::new(CountingSearch::default());
    let registry = registry(
        &search,
        serde_json::json!([
            {"type": "web_search"},
            {"type": "function", "name": "get_weather", "parameters": {"type": "object"}}
        ]),
    )
    .await;
    let items = [
        call("get_weather", "w1"),
        call("get_weather", "w2"),
        call("web_search", "s"),
    ];
    let mut budget = limit(1);

    let (scheduler, results) = plan_and_run(&items, &registry, &mut budget).await;

    assert_eq!(results.len(), 1, "only gateway-executed calls are scheduled");
    assert_eq!(search.executions.load(Ordering::SeqCst), 1);
    assert!(!budget.has_refused());
    assert_eq!(scheduler.public_item_index(2), Some(2));
}

#[tokio::test]
async fn omitted_refusals_keep_later_public_indexes_contiguous() {
    let search = Arc::new(CountingSearch::default());
    let registry = registry(
        &search,
        serde_json::json!([{"type": "web_search"}, {"type": "file_search", "vector_store_ids": ["vs"]}]),
    )
    .await;
    // The refused file search has no public item, so the trailing message moves up.
    let message: OutputItem = serde_json::from_value(serde_json::json!({
        "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "done", "annotations": []}]
    }))
    .expect("message item");
    let items = [call("web_search", "a"), call("file_search", "b"), message];
    let mut budget = limit(1);
    let mut scheduler =
        GatewayScheduler::plan_with_budget(&items, &registry, 5, GatewaySchedulerPolicy::default(), &mut budget);

    assert_eq!(scheduler.public_item_index(0), Some(0));
    assert_eq!(scheduler.public_item_index(1), None);
    assert_eq!(scheduler.public_item_index(2), Some(1));
    let results = scheduler.execute().await.expect("round executes");
    assert!(results[1].omitted);
    let public = public_output_items(&items, &registry, &results).expect("public projection");
    assert_eq!(public.len(), 2);
    assert!(matches!(public[1], OutputItem::Message(_)));
}

#[tokio::test]
async fn refused_web_search_completes_at_searching_without_a_completed_event() {
    let search = Arc::new(CountingSearch::default());
    let registry = registry(&search, serde_json::json!([{"type": "web_search"}])).await;
    let items = [call("web_search", "a"), call("web_search", "b")];
    let mut budget = limit(1);
    let (scheduler, results) = plan_and_run(&items, &registry, &mut budget).await;

    let (sender, mut receiver) = mpsc::channel(64);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    emit_gateway_start_events(scheduler.event_plans(), &mut relay)
        .await
        .expect("start events");
    emit_gateway_completed_events(&results, scheduler.event_plans(), &mut relay)
        .await
        .expect("completed events");

    let events: Vec<Value> = received_events(&mut receiver)
        .into_iter()
        .filter(|event| event["output_index"] == 1)
        .collect();
    let types: Vec<_> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        types,
        [
            "response.output_item.added",
            "response.web_search_call.in_progress",
            "response.web_search_call.searching",
            "response.output_item.done",
        ]
    );
    assert_eq!(events[3]["item"]["status"], "searching");
}

fn received_events(receiver: &mut mpsc::Receiver<StreamEvent>) -> Vec<Value> {
    std::iter::from_fn(|| receiver.try_recv().ok().map(StreamEvent::into_frame))
        .map(|frame| {
            let data = frame
                .content
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data line");
            serde_json::from_str(data).expect("event JSON")
        })
        .collect()
}

#[tokio::test]
async fn an_omitted_refusal_leaves_the_public_slot_for_a_later_one() {
    let search = Arc::new(CountingSearch::default());
    let registry = registry(
        &search,
        serde_json::json!([{"type": "web_search"}, {"type": "file_search", "vector_store_ids": ["vs"]}]),
    )
    .await;
    let items = [
        call("web_search", "a"),
        call("file_search", "b"),
        call("web_search", "c"),
        call("web_search", "d"),
    ];
    let mut budget = limit(1);

    let (scheduler, results) = plan_and_run(&items, &registry, &mut budget).await;

    assert!(
        results[1].omitted,
        "a refused call without a started item has no public item"
    );
    assert!(
        matches!(&results[2].public_output, Some(OutputItem::WebSearchCall(call)) if call.status == WebSearchCallStatus::Searching)
    );
    assert!(results[3].omitted, "only one refused call is public");
    assert_eq!(scheduler.public_item_index(2), Some(1));
    assert_eq!(search.executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refused_code_interpreter_completes_at_interpreting_without_a_completed_event() {
    let started: OutputItem = serde_json::from_value(serde_json::json!({
        "type": "code_interpreter_call", "id": "ci_1", "container_id": "cntr_1",
        "code": "print(1)", "status": "in_progress", "outputs": null
    }))
    .expect("code-interpreter item");
    let refused = admission::refused_output(&started);
    assert!(
        matches!(&refused, Some(OutputItem::CodeInterpreterCall(call)) if call.status == crate::types::io::CodeInterpreterCallStatus::Interpreting)
    );
    let plan = GatewayEventPlan {
        output_index: 0,
        started_output: Some(started),
        completed_output: refused,
        arguments: None,
    };

    let (sender, mut receiver) = mpsc::channel(64);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    emit_gateway_start_events(std::iter::once(&plan), &mut relay)
        .await
        .expect("start events");
    emit_gateway_completed_events::<GatewayCallResult>(&[], std::iter::once(&plan), &mut relay)
        .await
        .expect("completed events");

    let events = received_events(&mut receiver);
    let types: Vec<_> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        !types.contains(&"response.code_interpreter_call.completed"),
        "{types:?}"
    );
    assert_eq!(types.first(), Some(&"response.output_item.added"));
    assert!(types.contains(&"response.code_interpreter_call.interpreting"));
    let done = events.last().expect("output_item.done");
    assert_eq!(done["type"], "response.output_item.done");
    assert_eq!(done["item"]["status"], "interpreting");
}
