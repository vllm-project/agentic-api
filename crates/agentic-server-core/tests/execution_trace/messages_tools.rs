//! Gateway-executed built-in tools on the Messages path: each call that runs
//! is an `agentic.tool.execute` stage typed by the registry entry that served
//! it, and a call refused at admission is no stage at all.

use super::*;
use crate::messages_gateway_tools::{self as gateway_tools, Offer, QUERIES};

#[tokio::test]
async fn messages_loop_gateway_tool_is_a_typed_stage_between_rounds() {
    assert_one_typed_tool_stage(false).await;
}

#[tokio::test]
async fn messages_stream_gateway_tool_is_a_typed_stage_between_rounds() {
    assert_one_typed_tool_stage(true).await;
}

async fn assert_one_typed_tool_stage(stream: bool) {
    let traces = Traces::new();
    let (search_url, search_task) = support::search_server(support::Search::Answers).await;
    let fixture = messages_fixture(vec![
        gateway_tools::tool_round(stream, Offer::Native),
        gateway_tools::final_round(stream),
    ])
    .await;
    let (exec_ctx, registry) =
        gateway_tools::with_web_search(ExecutionContext::clone(&fixture.exec_ctx), &search_url, Offer::Native).await;
    if stream {
        let response = run_messages_stream(
            gateway_tools::request(true, Offer::Native),
            registry,
            exec_ctx,
            fixture.upstream.clone(),
        )
        .instrument(traces.root.clone())
        .await
        .unwrap();
        // Drained outside the request's span: the tools run while the client
        // polls, so the stream must carry the execution's context itself.
        let frames = tokio::spawn(
            response
                .body
                .collect::<Vec<_>>()
                .instrument(tracing::info_span!(parent: None, "unrelated.consumer")),
        )
        .await
        .unwrap();
        assert!(
            frames.iter().any(|frame| frame.starts_with("event: message_stop")),
            "{frames:?}"
        );
    } else {
        let response = run_messages_loop(
            gateway_tools::request(false, Offer::Native),
            &registry,
            &exec_ctx,
            &fixture.upstream,
        )
        .instrument(traces.root.clone())
        .await
        .unwrap();
        assert_eq!(response.body["stop_reason"], "end_turn");
    }
    search_task.abort();
    let _ = search_task.await;

    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 2, "one round per model turn");
    let fed_back = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    assert_eq!(fed_back[0]["is_error"], false, "the first search ran: {fed_back}");
    assert_eq!(
        fed_back[1]["is_error"], true,
        "the second was refused by max_uses: {fed_back}"
    );

    let spans = traces
        .finished_by_name(&["agentic.execute", "agentic.tool.execute"])
        .await;
    trace_attributes::assert_allowed(&spans, &QUERIES);
    let execute = spans.iter().find(|span| span.name == "agentic.execute").unwrap();
    let tools: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "agentic.tool.execute")
        .collect();
    assert_eq!(tools.len(), 1, "the refused call is not a stage");
    let tool = tools[0];
    assert_eq!(attribute(tool, "agentic.tool.type"), Some(&Value::from("web_search")));
    assert_eq!(tool.parent_span_id, execute.span_context.span_id());
    let round = |index: i64| {
        spans
            .iter()
            .find(|span| {
                span.name == "agentic.inference_round"
                    && attribute(span, "agentic.inference.round") == Some(&Value::I64(index))
            })
            .unwrap()
    };
    assert!(round(0).end_time <= tool.start_time, "the tool runs after its round");
    assert!(tool.end_time <= round(1).start_time, "and before the next one");
}
