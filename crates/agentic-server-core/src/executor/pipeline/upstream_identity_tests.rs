//! Model metadata follows the same ingestion transitions in both validation policies.

use super::*;
use crate::executor::error::ResourceLimit;
use crate::executor::response_budget::RETAINED_CONTAINER_OVERHEAD_BYTES;
use crate::executor::upstream::tests::request_context;
use crate::types::upstream_identity::{MAX_UPSTREAM_MODEL_BYTES, UpstreamModelError};
use serde_json::{Value, json};

fn round(validation: Validation, budget: Option<ExecutorResponseBudget>) -> RoundIngestion {
    RoundIngestion::new("local".into(), None, validation, TranslationContext::default(), budget)
}

fn push(round: &mut RoundIngestion, kind: &str, model: Option<Value>) -> ExecutorResult<()> {
    let status = match kind {
        "created" | "in_progress" => "in_progress",
        terminal => terminal,
    };
    let mut response = json!({"id":"upstream","status":status,"output":[]});
    if let Some(model) = model {
        response["model"] = model;
    }
    let event = json!({"type":format!("response.{kind}"),"response":response});
    round.push(SseLine::parse(&format!("data: {event}")))?;
    Ok(())
}

fn start(round: &mut RoundIngestion, model: Option<Value>) {
    push(round, "created", model.clone()).unwrap();
    push(round, "in_progress", model).unwrap();
}

fn assert_invalid(error: &ExecutorError) {
    assert!(matches!(
        error,
        ExecutorError::UpstreamModel(UpstreamModelError::Invalid)
    ));
    assert_eq!(error.http_status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(error.error_type(), "upstream_error");
    assert!(!error.to_string().contains("sensitive"));
    assert!(!format!("{error:?}").contains("sensitive"));
}

#[test]
fn json_and_sse_report_exact_terminal_model_without_changing_public_model() {
    for validation in [Validation::Strict, Validation::Lenient] {
        for status in ["completed", "failed", "incomplete"] {
            let model = "provider/snapshot-2026-01-01";
            let mut stream = round(validation, None);
            start(&mut stream, Some(json!(model)));
            push(&mut stream, status, Some(json!(model))).unwrap();
            let stream = stream.finish("requested-alias", None, None).unwrap();

            let mut json = round(validation, None);
            json.load_json_body(&json!({"id":"upstream","status":status,"model":model,"output":[]}).to_string())
                .unwrap();
            let json = json.finish("requested-alias", None, None).unwrap();
            for result in [&json, &stream] {
                assert_eq!(result.upstream_model.as_ref().unwrap().as_str(), model);
                assert_eq!(result.payload.model, "requested-alias");
                assert!(!serde_json::to_string(&result.payload).unwrap().contains(model));
            }
        }
    }
}

#[test]
fn only_explicit_terminal_model_is_evidence() {
    for validation in [Validation::Strict, Validation::Lenient] {
        for absent in [None, Some(Value::Null)] {
            let mut stream = round(validation, None);
            start(&mut stream, Some(json!("known-early")));
            push(&mut stream, "completed", absent.clone()).unwrap();
            assert!(
                stream
                    .finish("known-early", None, None)
                    .unwrap()
                    .upstream_model
                    .is_none()
            );

            let mut response = json!({"id":"upstream","status":"completed","output":[]});
            if let Some(value) = absent {
                response["model"] = value;
            }
            let mut json = round(validation, None);
            json.load_json_body(&response.to_string()).unwrap();
            assert!(json.finish("requested", None, None).unwrap().upstream_model.is_none());
        }
        let mut stream = round(validation, None);
        start(&mut stream, None);
        push(&mut stream, "completed", Some(json!("late-model"))).unwrap();
        assert_eq!(
            stream
                .finish("requested", None, None)
                .unwrap()
                .upstream_model
                .unwrap()
                .as_str(),
            "late-model"
        );
    }
    for status in [None, Some("in_progress"), Some("unrecognized")] {
        let mut response = json!({"id":"upstream","model":"not-terminal","output":[]});
        if let Some(status) = status {
            response["status"] = json!(status);
        }
        let mut json = round(Validation::Lenient, None);
        json.load_json_body(&response.to_string()).unwrap();
        assert!(json.finish("requested", None, None).unwrap().upstream_model.is_none());
    }
    let mut stream = round(Validation::Lenient, None);
    start(&mut stream, Some(json!("early-model")));
    stream.push(ClassifiedSseLine::Done).unwrap();
    assert!(stream.finish("requested", None, None).unwrap().upstream_model.is_none());
    let mut stream = round(Validation::Strict, None);
    start(&mut stream, Some(json!("early-model")));
    assert!(stream.finish("requested", None, None).is_err());
}

#[test]
fn malformed_metadata_fails_closed_and_conflicts_follow_validation_policy() {
    for validation in [Validation::Strict, Validation::Lenient] {
        for model in [
            json!(""),
            json!(" \t"),
            json!(false),
            json!(42),
            json!(["sensitive"]),
            json!({"sensitive":"value"}),
            json!("sensitive".repeat(MAX_UPSTREAM_MODEL_BYTES)),
        ] {
            let mut json = round(validation, None);
            let result = json
                .load_json_body(&json!({"id":"upstream","status":"completed","model":model,"output":[]}).to_string());
            match validation {
                Validation::Strict => assert_invalid(&result.unwrap_err()),
                Validation::Lenient => {
                    result.unwrap();
                    assert!(json.finish("requested", None, None).unwrap().upstream_model.is_none());
                }
            }
            for kind in ["created", "in_progress", "completed"] {
                let mut stream = round(validation, None);
                if kind != "created" {
                    push(&mut stream, "created", None).unwrap();
                }
                if kind == "completed" {
                    push(&mut stream, "in_progress", None).unwrap();
                }
                let result = push(&mut stream, kind, Some(model.clone()));
                match validation {
                    Validation::Strict => assert_invalid(&result.unwrap_err()),
                    Validation::Lenient => {
                        result.unwrap();
                        if kind == "completed" {
                            assert!(stream.finish("requested", None, None).unwrap().upstream_model.is_none());
                        }
                    }
                }
            }
        }
        for kind in ["in_progress", "completed", "failed", "incomplete"] {
            let mut stream = round(validation, None);
            push(&mut stream, "created", Some(json!("sensitive-first"))).unwrap();
            if kind != "in_progress" {
                push(&mut stream, "in_progress", None).unwrap();
            }
            let result = push(&mut stream, kind, Some(json!("sensitive-second")));
            match validation {
                Validation::Strict => {
                    let error = result.unwrap_err();
                    assert!(matches!(
                        error,
                        ExecutorError::UpstreamModel(UpstreamModelError::Changed)
                    ));
                    assert_eq!(error.http_status(), http::StatusCode::BAD_GATEWAY);
                    assert!(!format!("{error:?}").contains("sensitive"));
                }
                Validation::Lenient => {
                    result.unwrap();
                    if kind == "in_progress" {
                        // Returning to the original spelling cannot restore evidence.
                        push(&mut stream, "completed", Some(json!("sensitive-first"))).unwrap();
                    }
                    assert!(stream.finish("requested", None, None).unwrap().upstream_model.is_none());
                }
            }
        }
    }
}

#[test]
fn terminal_evidence_cannot_be_replaced_or_followed_by_more_events() {
    for validation in [Validation::Strict, Validation::Lenient] {
        for later in ["created", "in_progress", "completed", "failed", "incomplete"] {
            let mut stream = round(validation, None);
            start(&mut stream, Some(json!("model")));
            push(&mut stream, "completed", Some(json!("model"))).unwrap();
            let result = push(&mut stream, later, Some(json!("model")));
            match validation {
                Validation::Strict => assert!(result.unwrap_err().to_string().contains("after its terminal event")),
                Validation::Lenient => {
                    result.unwrap();
                    assert!(stream.finish("requested", None, None).unwrap().upstream_model.is_none());
                }
            }
        }
    }
    let mut stream = round(Validation::Strict, None);
    assert!(push(&mut stream, "completed", Some(json!("model"))).is_err());
}

#[test]
fn model_retention_is_charged_once_per_round_and_is_utf8_bounded() {
    let model = "é".repeat(MAX_UPSTREAM_MODEL_BYTES / 2);
    assert_eq!(UpstreamModelId::new(model.clone()).unwrap().as_str(), model);
    assert!(UpstreamModelId::new(format!("{model}é")).is_err());
    let expected = 2 * RETAINED_CONTAINER_OVERHEAD_BYTES + "upstream".len() + model.len();
    for streaming in [false, true] {
        let budget = ExecutorResponseBudget::with_limit(expected);
        let mut ingestion = round(Validation::Strict, Some(budget.clone()));
        if streaming {
            start(&mut ingestion, Some(json!(model)));
            push(&mut ingestion, "completed", Some(json!(model))).unwrap();
        } else {
            ingestion
                .load_json_body(&json!({"id":"upstream","status":"completed","model":model,"output":[]}).to_string())
                .unwrap();
        }
        assert_eq!(budget.used(), expected);
        assert!(
            ingestion
                .finish("request", None, None)
                .unwrap()
                .upstream_model
                .is_some()
        );
        let mut next = round(Validation::Strict, Some(budget));
        let error = if streaming {
            push(&mut next, "created", Some(json!(model))).unwrap_err()
        } else {
            next.load_json_body(&json!({"id":"upstream","status":"completed","model":model,"output":[]}).to_string())
                .unwrap_err()
        };
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                ..
            }
        ));
    }
    assert!(!format!("{:?}", UpstreamModelId::new("sensitive".into()).unwrap()).contains("sensitive"));
}

#[tokio::test]
async fn evidence_is_round_scoped_and_transport_errors_cannot_finalize_it() {
    let mut agent = AgentPipeline::new(request_context(), None, None);
    for model in [Some("snapshot-a"), None, Some("snapshot-b")] {
        let body = json!({"id":"upstream","status":"completed","model":model,"output":[]});
        let result = agent
            .run_with_json_body(
                &body.to_string(),
                Validation::Strict,
                TranslationContext::default(),
                None,
            )
            .unwrap();
        assert_eq!(result.upstream_model.as_ref().map(UpstreamModelId::as_str), model);
        let events = ["created", "in_progress", "completed"].map(|kind| {
            let status = if kind == "completed" {
                "completed"
            } else {
                "in_progress"
            };
            Ok(format!(
                "data: {}",
                json!({"type":format!("response.{kind}"),"response":{
                    "id":"upstream","status":status,"model":model,"output":[]
                }})
            ))
        });
        let result = agent
            .run_with_stream_body(
                futures::stream::iter(events),
                Validation::Strict,
                TranslationContext::default(),
                &ToolRegistry::default(),
                0,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.upstream_model.as_ref().map(UpstreamModelId::as_str), model);
    }
    let body = futures::stream::iter([
        Ok(format!(
            "data: {}",
            json!({"type":"response.created","response":{"id":"upstream","status":"in_progress","model":"snapshot-c"}})
        )),
        Err(ExecutorError::StreamError("upstream disconnected".into())),
    ]);
    let result = agent
        .run_with_stream_body(
            body,
            Validation::Strict,
            TranslationContext::default(),
            &ToolRegistry::default(),
            0,
            None,
        )
        .await;
    assert!(result.is_err());
    assert!(agent.round.is_some(), "failed round was not finalized");
}
