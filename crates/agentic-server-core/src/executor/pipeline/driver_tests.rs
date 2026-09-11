use super::*;
use crate::executor::upstream::tests::request_context;
use crate::tool::ToolType;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

fn line(event: &Value) -> String {
    format!("data: {event}")
}

fn search_context() -> TranslationContext {
    TranslationContext::new(
        HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]),
        HashSet::from(["hidden".to_owned()]),
        true,
    )
}

#[tokio::test]
async fn live_delivery_applies_backpressure_and_disconnect_stops_input() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let mut agent = AgentPipeline::new(request_context(), None, Some(sender));
    let polled = AtomicUsize::new(0);
    let body = futures::stream::iter([
        Ok(line(
            &json!({"type":"response.created","response":{"id":"upstream","status":"in_progress"}}),
        )),
        Ok(line(
            &json!({"type":"response.in_progress","response":{"id":"upstream","status":"in_progress"}}),
        )),
        Err(ExecutorError::StreamError("must not read ahead".to_owned())),
    ])
    .inspect(|_| {
        polled.fetch_add(1, Ordering::SeqCst);
    });
    let registry = ToolRegistry::default();
    let mut run =
        Box::pin(agent.run_with_stream_body(body, Validation::Lenient, TranslationContext::default(), &registry, 0));
    assert!(futures::poll!(run.as_mut()).is_pending());
    assert_eq!(
        polled.load(Ordering::SeqCst),
        2,
        "full sender blocks further upstream reads"
    );
    let first = receiver.try_recv().expect("created is delivered before upstream EOF");
    assert!(first.content.contains("resp_test"));
    drop(receiver);
    let error = run.await.err().expect("disconnect fails the runner");
    assert!(error.to_string().contains("stream receiver closed"));
    assert_eq!(polled.load(Ordering::SeqCst), 2);
    assert!(agent.round.is_some(), "failed input must not be finalized");
}

#[tokio::test]
async fn gateway_sequence_and_lifecycle_survive_rounds_while_items_start_fresh() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    let mut agent = AgentPipeline::new(request_context(), None, Some(sender));
    let registry = ToolRegistry::default();
    for round in 0..2 {
        let item = json!({"id":format!("fc_{round}"),"type":"function_call","call_id":format!("call_{round}"),"name":"echo","arguments":"{}","status":"completed"});
        let events = [
            json!({"type":"response.created","response":{"id":"upstream","status":"in_progress"}}),
            json!({"type":"response.in_progress","response":{"id":"upstream","status":"in_progress"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"upstream","status":"completed","output":[item]}}),
        ];
        let result = agent
            .run_with_stream_body(
                futures::stream::iter(events.iter().map(|event| Ok(line(event)))),
                Validation::Lenient,
                TranslationContext::default(),
                &registry,
                round,
            )
            .await
            .unwrap();
        assert_eq!(result.payload.id, "resp_test");
        assert_eq!(result.payload.output.len(), 1, "each round owns fresh item slots");
        assert!(result.deferred_events.is_empty());
        assert!(agent.round.is_none());
    }
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    assert_eq!(
        events.iter().map(|event| event.sequence_number).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    let indices = events
        .iter()
        .filter_map(|event| {
            event
                .content
                .lines()
                .find_map(|line| crate::events::normalize_sse_line(line).and_then(|frame| frame.wire.output_index))
        })
        .collect::<Vec<_>>();
    assert_eq!(indices, [0, 1]);
}

#[tokio::test]
async fn json_and_sse_preserve_the_same_terminal_metadata_and_request_ids() {
    for status in ["completed", "incomplete", "failed"] {
        let response = json!({"id":"upstream","status":status,"output":[],"error":{"code":"test","message":"detail"},"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}});
        let mut agent = AgentPipeline::new(request_context(), None, None);
        let mut from_json = serde_json::to_value(
            agent
                .run_with_json_body(&response.to_string(), Validation::Strict, TranslationContext::default())
                .unwrap(),
        )
        .unwrap();
        let events = [
            json!({"type":"response.created","response":{"id":"upstream","status":"in_progress"}}),
            json!({"type":"response.in_progress","response":{"id":"upstream","status":"in_progress"}}),
            json!({"type":format!("response.{status}"),"response":response}),
        ];
        let result = agent
            .run_with_stream_body(
                futures::stream::iter(events.iter().map(|event| Ok(line(event)))),
                Validation::Strict,
                TranslationContext::default(),
                &ToolRegistry::default(),
                0,
            )
            .await
            .unwrap();
        let mut from_sse = serde_json::to_value(result.payload).unwrap();
        from_json.as_object_mut().unwrap().remove("created_at");
        from_sse.as_object_mut().unwrap().remove("created_at");
        assert_eq!(from_json, from_sse);
        assert_eq!(from_json["id"], "resp_test");
    }
}

#[test]
fn json_preserves_nonterminal_status_and_strict_validation_rejects_it() {
    let body = r#"{"id":"upstream","status":"in_progress","output":[]}"#;
    let mut agent = AgentPipeline::new(request_context(), None, None);
    assert_eq!(
        agent
            .run_with_json_body(body, Validation::Lenient, TranslationContext::default())
            .unwrap()
            .status,
        "in_progress"
    );
    assert!(
        agent
            .run_with_json_body(body, Validation::Strict, TranslationContext::default())
            .unwrap_err()
            .to_string()
            .contains("is not terminal")
    );
}

#[test]
fn json_search_validation_precedes_lenient_item_loading() {
    for item in [
        json!({"type":"function_call","name":"tool_search","arguments":"{}","status":"completed"}),
        json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"hidden","arguments":"{}","status":"completed"}),
    ] {
        let mut agent = AgentPipeline::new(request_context(), None, None);
        let body = json!({"id":"upstream","status":"completed","output":[item]}).to_string();
        assert!(
            agent
                .run_with_json_body(&body, Validation::Lenient, search_context())
                .is_err()
        );
    }
}

#[test]
fn json_search_projection_handles_completed_and_aborted_calls() {
    for status in ["completed", "incomplete", "failed"] {
        let (arguments, call_status) = if status == "completed" {
            ("[\"weather\"]", "completed")
        } else {
            ("{\"query\":", "in_progress")
        };
        let body = json!({"id":"upstream","status":status,"output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool_search","arguments":arguments,"status":call_status}]}).to_string();
        let mut agent = AgentPipeline::new(request_context(), None, None);
        let payload = agent
            .run_with_json_body(&body, Validation::Lenient, search_context())
            .unwrap();
        let output = serde_json::to_value(payload.output).unwrap();
        if status == "failed" {
            assert_eq!(output, json!([]));
        } else {
            assert_eq!(output[0]["type"], "tool_search_call");
            assert_eq!(output[0]["status"], status);
            assert_eq!(
                output[0]["arguments"],
                if status == "completed" {
                    json!(["weather"])
                } else {
                    json!({})
                }
            );
        }
    }
}

#[test]
fn prepared_search_state_survives_rounds_and_is_taken_once_for_persistence() {
    let mut request = request_context();
    request.enriched_request.tools = Some(
        serde_json::from_value(json!([
            {"type":"tool_search","execution":"client"},
            {"type":"function","name":"weather","defer_loading":true},
            {"type":"function","name":"hidden","defer_loading":true}
        ]))
        .unwrap(),
    );
    request.enriched_request.parallel_tool_calls = Some(false);
    request.original_request = request.enriched_request.clone();
    let unprepared = AgentPipeline::new(request, None, None);
    assert!(
        unprepared
            .ensure_request_prepared()
            .unwrap_err()
            .to_string()
            .contains("require prepared request-scoped state")
    );
    let (mut request, _) = unprepared.into_parts();
    let loaded: Vec<crate::types::tools::ResponsesTool> =
        serde_json::from_value(json!([{"type":"function","name":"weather","defer_loading":true}])).unwrap();
    let mut state = ToolSearchState::build_with_loaded_tools(&request.enriched_request, &loaded, false).unwrap();
    state.prepare_inference_request(&mut request.enriched_request).unwrap();
    let mut agent = AgentPipeline::new(request, Some(state), None);
    for _ in 0..2 {
        agent.ensure_request_prepared().unwrap();
        let state = agent.tool_search_state().expect("request keeps prepared search state");
        assert_eq!(state.loaded_public_tools().len(), 1);
        assert!(state.withheld_function_names().contains("hidden"));
        assert!(!state.withheld_function_names().contains("weather"));
        agent
            .run_with_json_body(
                r#"{"id":"upstream","status":"completed","output":[]}"#,
                Validation::Lenient,
                search_context(),
            )
            .unwrap();
    }
    let metadata = agent
        .take_tool_search_metadata()
        .expect("search metadata survives both rounds");
    assert_eq!(metadata.loaded_tools.len(), 1);
    assert_eq!(
        serde_json::to_value(&metadata.loaded_tools).unwrap()[0]["name"],
        "weather"
    );
    assert_eq!(metadata.effective_tools.unwrap().len(), 3);
    assert!(agent.tool_search_state().is_none());
    assert!(agent.take_tool_search_metadata().is_none());
}
