//! Replay/fault tests plus pinned reference characterization in `qualification`.
//! All provider response bytes originate in recorder-generated cassettes.

mod qualification;

use super::*;
use crate::executor::replay::prepare_initial_reasoning;
use crate::types::{InputItem, OutputItem, ReasoningTextContent, ResponsesInput};
use serde::Deserialize;
use serde_json::{Value, json};

const OPAQUE: ReasoningReplayPolicy = ReasoningReplayPolicy::OpaqueResponses;

#[test]
fn redaction_preserves_safe_tool_search_lifecycle_errors() {
    for error in [
        crate::tool::ToolError::InvalidUpstreamToolSearch,
        crate::tool::ToolError::UpstreamWithheldFunctionCall,
    ] {
        let error = provider_result::<()>(OPAQUE, Err(error.into())).unwrap_err();
        assert!(error.is_invalid_upstream_tool_search());
        assert_eq!(error.http_status(), http::StatusCode::BAD_GATEWAY);
    }
}

#[derive(Deserialize)]
struct Recording {
    turns: Vec<RecordedTurn>,
}

#[derive(Deserialize)]
struct RecordedTurn {
    response: RecordedResponse,
}

#[derive(Deserialize)]
struct RecordedResponse {
    body: Option<Value>,
    sse: Option<Vec<String>>,
}

fn recorded(stream: bool) -> RecordedResponse {
    let yaml = if stream {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/reasoning/responses/reasoning-openai-reference-gpt-5.6-streaming.yaml"
        ))
    } else {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/reasoning/responses/reasoning-openai-reference-gpt-5.6-nonstreaming.yaml"
        ))
    };
    serde_yaml::from_str::<Recording>(yaml)
        .unwrap()
        .turns
        .remove(0)
        .response
}

async fn ingest_stream(lines: Vec<ExecutorResult<String>>) -> ExecutorResult<StreamPayload> {
    let mut agent = agent_pipeline(tests::request_context(), None, None);
    provider_result(
        OPAQUE,
        agent
            .run_with_stream_body(
                futures::stream::iter(lines),
                validation_for_policy(OPAQUE),
                TranslationContext::default(),
                &ToolRegistry::default(),
                0,
                None,
            )
            .await,
    )
}

#[tokio::test]
async fn recorded_json_and_sse_use_the_existing_strict_path_then_project_without_mutation() {
    for stream in [false, true] {
        let response = recorded(stream);
        let payload = if stream {
            ingest_stream(
                response
                    .sse
                    .unwrap()
                    .join("")
                    .lines()
                    .map(|line| Ok(line.to_owned()))
                    .collect(),
            )
            .await
            .unwrap()
            .payload
        } else {
            let mut agent = agent_pipeline(tests::request_context(), None, None);
            agent
                .run_with_json_body(
                    &response.body.unwrap().to_string(),
                    validation_for_policy(OPAQUE),
                    TranslationContext::default(),
                    None,
                )
                .unwrap()
                .payload
        };
        let mut context = tests::request_context();
        context.enriched_request.input =
            ResponsesInput::Items(payload.output.iter().filter_map(OutputItem::to_input_item).collect());
        let canonical = serde_json::to_value(&context.enriched_request).unwrap();
        let wire: Value = serde_json::from_str(&request_for_policy(&context, stream, OPAQUE).unwrap()).unwrap();
        assert_eq!(wire["store"], false);
        let source = canonical["input"].as_array().unwrap();
        let projected = wire["input"].as_array().unwrap();
        assert_eq!(source.len(), projected.len());
        let mut reasoning_count = 0;
        for (original, actual) in source.iter().zip(projected) {
            if original["type"] == "reasoning" {
                reasoning_count += 1;
                assert!(
                    original["encrypted_content"]
                        .as_str()
                        .is_some_and(|state| !state.is_empty())
                );
                for field in ["id", "summary", "encrypted_content"] {
                    assert_eq!(actual[field], original[field]);
                }
                assert!(actual.get("content").is_none());
                assert!(actual.get("replay_provenance").is_none());
            } else {
                assert_eq!(actual, original);
            }
        }
        assert_eq!(reasoning_count, 1);
        assert_eq!(serde_json::to_value(&context.enriched_request).unwrap(), canonical);
    }
}

#[test]
fn candidate_selects_strict_json_validation_and_redacts_the_retained_cause() {
    assert!(matches!(
        validation_for_policy(ReasoningReplayPolicy::VllmPlaintext),
        Validation::Lenient
    ));
    assert!(matches!(validation_for_policy(OPAQUE), Validation::Strict));
    let mut body = recorded(false).body.unwrap();
    body["output"][0]["status"] = json!("reflected-opaque-secret");
    let mut agent = agent_pipeline(tests::request_context(), None, None);
    let result = agent.run_with_json_body(
        &body.to_string(),
        validation_for_policy(OPAQUE),
        TranslationContext::default(),
        None,
    );
    let error = provider_result(OPAQUE, result).unwrap_err();
    assert_eq!(error.http_status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(error.error_code(), "invalid_upstream_response");
    assert!(!format!("{error:?} {error}").contains("reflected-opaque-secret"));
    assert!(!error.response_error().to_string().contains("reflected-opaque-secret"));
    assert!(
        std::error::Error::source(&error).is_some(),
        "typed source retained for explicit inspection"
    );
}

#[tokio::test]
async fn recorded_stream_rejects_missing_terminal_duplicate_completion_and_disconnect() {
    let wire = recorded(true).sse.unwrap().join("");
    let lines: Vec<_> = wire.lines().map(str::to_owned).collect();
    let terminal = lines
        .iter()
        .position(|line| {
            line.contains("\"type\":\"response.completed\"") || line.contains("\"type\": \"response.completed\"")
        })
        .unwrap();
    assert!(
        ingest_stream(lines[..terminal].iter().cloned().map(Ok).collect())
            .await
            .is_err()
    );
    let done = lines
        .iter()
        .position(|line| line.contains("response.output_item.done") && line.starts_with("data:"))
        .unwrap();
    let mut duplicate = lines.clone();
    duplicate.insert(done, lines[done].clone());
    assert!(ingest_stream(duplicate.into_iter().map(Ok).collect()).await.is_err());
    let mut out_of_order = lines.clone();
    let added = lines
        .iter()
        .position(|line| line.contains("response.output_item.added") && line.starts_with("data:"))
        .unwrap();
    out_of_order.swap(added, done);
    assert!(ingest_stream(out_of_order.into_iter().map(Ok).collect()).await.is_err());
    let mut wrong_index = lines.clone();
    let mut event: Value = serde_json::from_str(lines[done].strip_prefix("data:").unwrap()).unwrap();
    event["output_index"] = json!(99);
    wrong_index[done] = format!("data: {event}");
    assert!(ingest_stream(wrong_index.into_iter().map(Ok).collect()).await.is_err());
    let mut disconnect: Vec<_> = lines[..done].iter().cloned().map(Ok).collect();
    disconnect.push(Err(ExecutorError::StreamError("reflected-opaque-secret".into())));
    let error = ingest_stream(disconnect).await.unwrap_err();
    assert!(!format!("{error:?} {error}").contains("reflected-opaque-secret"));
}

#[test]
fn candidate_preparation_never_runs_the_plaintext_sanitizer() {
    let body = recorded(false).body.unwrap();
    let mut reasoning: crate::types::ReasoningOutput = serde_json::from_value(body["output"][0].clone()).unwrap();
    reasoning.content.push(ReasoningTextContent::new("plaintext"));
    let mut input = ResponsesInput::Items(vec![InputItem::Reasoning(reasoning)]);
    let before = serde_json::to_value(&input).unwrap();
    for round in [0, 1] {
        prepare_initial_reasoning(&mut input, OPAQUE, round, false).unwrap();
        assert_eq!(serde_json::to_value(&input).unwrap(), before);
    }
    prepare_initial_reasoning(&mut input, ReasoningReplayPolicy::VllmPlaintext, 0, false).unwrap();
    let vllm = serde_json::to_value(&input).unwrap();
    assert!(vllm[0]["encrypted_content"].is_null());
    assert_eq!(vllm[0]["content"], before[0]["content"]);
}

#[tokio::test]
async fn candidate_delivery_backpressure_and_cancellation_drop_inline_input() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    for disconnect in [false, true] {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let mut agent = agent_pipeline(tests::request_context(), None, Some(sender));
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&dropped));
        let polled = Arc::clone(&polls);
        let wire = recorded(true).sse.unwrap().join("");
        let body = async_stream::stream! {
            let _guard = guard;
            for line in wire.lines().filter(|line| line.starts_with("data:")) {
                polled.fetch_add(1, Ordering::SeqCst);
                yield Ok(line.to_owned());
            }
        };
        let registry = ToolRegistry::default();
        let mut run = Box::pin(agent.run_with_stream_body(
            body,
            validation_for_policy(OPAQUE),
            TranslationContext::default(),
            &registry,
            0,
            None,
        ));
        assert!(futures::poll!(run.as_mut()).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 2, "delivery bounds upstream read-ahead");
        if disconnect {
            drop(receiver);
            assert!(provider_result(OPAQUE, run.await).is_err());
        } else {
            drop(run);
        }
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }
}
