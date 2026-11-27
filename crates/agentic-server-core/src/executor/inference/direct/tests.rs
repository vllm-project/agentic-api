use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use futures::stream;
use serde_json::{Value, json};

use super::*;
use crate::events::WireEvent;
use crate::executor::pipeline::AgentPipeline;
use crate::executor::relay::StreamRelay;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::response_events::ResponseEventSink;
use crate::executor::upstream::{fetch_stream_payload, tests::request_context};
use crate::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler, ResponseSession};
use crate::storage::{ConversationStore, ResponseStore};
use crate::tool::ToolRegistry;
use crate::types::request_response::RequestPayload;

#[derive(Debug)]
struct PendingBackend {
    control: Mutex<Option<GenerationControl>>,
    pending_admission: bool,
    event: Option<WireEvent>,
}

impl DirectInferenceBackend for PendingBackend {
    fn generate<'a>(
        &'a self,
        _request: UpstreamRequest<'a>,
        control: GenerationControl,
    ) -> BoxFuture<'a, ExecutorResult<EventStream>> {
        *self.control.lock().unwrap() = Some(control);
        Box::pin(async move {
            if self.pending_admission {
                futures::future::pending::<()>().await;
            }
            let stream: EventStream = match &self.event {
                Some(wire) => Box::pin(stream::iter([
                    EventFrame::try_from(wire.clone()).map_err(ExecutorError::from)
                ])),
                None => Box::pin(stream::pending()),
            };
            Ok(stream)
        })
    }
}

impl PendingBackend {
    fn cancelled(&self) -> bool {
        self.control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cancellation
            .is_cancelled()
    }
}

fn request() -> RequestPayload {
    serde_json::from_value(json!({"model":"test", "input":"hello", "store":false})).unwrap()
}

#[tokio::test]
async fn dropping_admission_or_an_unpolled_generation_cancels_engine_work() {
    let request = request();
    for pending_admission in [true, false] {
        let backend = Arc::new(PendingBackend {
            control: Mutex::new(None),
            pending_admission,
            event: None,
        });
        let injected: Arc<dyn DirectInferenceBackend> = backend.clone();
        let mut admission = Box::pin(call(
            &injected,
            request.to_upstream_request(true, &HashMap::default()).unwrap(),
            "resp_test".to_owned(),
            Duration::ZERO,
            1024,
        ));
        if pending_admission {
            assert!(futures::poll!(admission.as_mut()).is_pending());
            assert!(!backend.cancelled());
            drop(admission);
        } else {
            let generation = admission.await.unwrap();
            assert!(!backend.cancelled());
            drop(generation);
        }
        assert!(backend.cancelled());
    }
}

#[tokio::test]
async fn admission_and_idle_timeouts_cancel_the_attempt() {
    let request = request();
    for pending_admission in [true, false] {
        let backend = Arc::new(PendingBackend {
            control: Mutex::new(None),
            pending_admission,
            event: None,
        });
        let injected: Arc<dyn DirectInferenceBackend> = backend.clone();
        let result = call(
            &injected,
            request.to_upstream_request(true, &HashMap::default()).unwrap(),
            "resp_test".to_owned(),
            Duration::from_millis(10),
            1024,
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(mut stream) => {
                let error = stream.next().await.unwrap().unwrap_err();
                drop(stream);
                error
            }
        };
        assert!(matches!(
            error,
            ExecutorError::LLMTransport {
                status: http::StatusCode::GATEWAY_TIMEOUT,
                ..
            }
        ));
        assert!(backend.cancelled());
    }
}

#[tokio::test]
async fn oversized_native_events_fail_before_ingestion() {
    let mut event = WireEvent::new("response.output_text.delta");
    event.rest.insert("delta".to_owned(), json!("x".repeat(1024)));
    let backend = Arc::new(PendingBackend {
        control: Mutex::new(None),
        pending_admission: false,
        event: Some(event),
    });
    let injected: Arc<dyn DirectInferenceBackend> = backend.clone();
    let request = request();
    let mut events = call(
        &injected,
        request.to_upstream_request(true, &HashMap::default()).unwrap(),
        "resp_test".to_owned(),
        Duration::ZERO,
        100,
    )
    .await
    .unwrap();
    assert!(matches!(
        events.next().await.unwrap(),
        Err(ExecutorError::ResourceLimitExceeded {
            limit: ResourceLimit::StreamEvent,
            max_bytes: 100,
        })
    ));
    assert!(events.next().await.is_none());
    assert!(backend.cancelled());
}

#[derive(Debug, Default)]
struct ConversationBackend {
    emit_call: bool,
    requests: Mutex<Vec<Value>>,
    controls: Mutex<Vec<GenerationControl>>,
}

impl DirectInferenceBackend for ConversationBackend {
    fn generate<'a>(
        &'a self,
        request: UpstreamRequest<'a>,
        control: GenerationControl,
    ) -> BoxFuture<'a, ExecutorResult<EventStream>> {
        Box::pin(async move {
            let mut requests = self.requests.lock().unwrap();
            requests.push(serde_json::to_value(request).unwrap());
            let emits_call = self.emit_call && requests.len() == 1;
            let item = if emits_call {
                json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"echo","arguments":"{\"input\":\"hi\"}","status":"completed"})
            } else {
                json!({"type":"message","id":"msg_2","role":"assistant","content":[{"type":"output_text","text":"done","annotations":[]}],"status":"completed"})
            };
            let mut added = item.clone();
            added["status"] = json!("in_progress");
            if emits_call {
                added["arguments"] = json!("");
            } else {
                added["content"] = json!([]);
            }
            let events = [
                json!({"type":"response.created","response":{"id":control.response_id,"status":"in_progress"}}),
                json!({"type":"response.in_progress","response":{"id":control.response_id,"status":"in_progress"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":added}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":{"id":control.response_id,"status":"completed","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}),
            ];
            self.controls.lock().unwrap().push(control);
            let frames = events.into_iter().map(|event| {
                EventFrame::try_from(serde_json::from_value::<WireEvent>(event).unwrap()).map_err(ExecutorError::from)
            });
            Ok(Box::pin(stream::iter(frames)) as EventStream)
        })
    }
}

#[tokio::test]
async fn direct_inference_reuses_custom_tool_translation_and_session_continuation() {
    let backend = Arc::new(ConversationBackend {
        emit_call: true,
        ..ConversationBackend::default()
    });
    let context = Arc::new(
        ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            "invalid-http-url".to_owned(),
        )
        .with_direct_inference(backend.clone()),
    );
    let session = ResponseSession::new(NonZeroUsize::new(100).unwrap(), NonZeroUsize::new(1024 * 1024).unwrap());
    let first: RequestPayload = serde_json::from_value(json!({
        "model":"test","input":"echo hi","tools":[{"type":"custom","name":"echo"}],"store":false,
    }))
    .unwrap();
    let first = ExecuteRequest::new(first, Arc::clone(&context))
        .with_session(&session, None)
        .unwrap()
        .run()
        .await
        .unwrap()
        .left()
        .unwrap();
    let output = serde_json::to_value(&first.output).unwrap();
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["input"], "hi");
    assert_eq!(first.usage.unwrap().total_tokens, 5);
    let second: RequestPayload = serde_json::from_value(json!({
        "model":"test","previous_response_id":first.id,"stream":true,"store":false,
        "tools":[{"type":"custom","name":"echo"}],
        "input":[{"type":"custom_tool_call_output","call_id":"call_1","output":"hi"}],
    }))
    .unwrap();
    let frames: Vec<String> = ExecuteRequest::new(second, context)
        .with_session(&session, None)
        .unwrap()
        .run()
        .await
        .unwrap()
        .right()
        .unwrap()
        .collect()
        .await;
    assert!(frames.iter().any(|frame| frame.contains("response.completed")));
    let requests = backend.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.get("previous_response_id").is_none() && request["store"] == false)
    );
    let history = requests[1]["input"].as_array().unwrap();
    assert!(
        history
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "call_1")
    );
    assert!(
        history
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["output"] == "hi")
    );
    let controls = backend.controls.lock().unwrap();
    assert_ne!(controls[0].request_id, controls[1].request_id);
    assert_ne!(controls[0].response_id, controls[1].response_id);
    assert!(controls.iter().all(|control| control.cancellation.is_cancelled()));
}

#[tokio::test]
async fn compaction_uses_the_injected_backend_and_existing_summary_processing() {
    let backend = Arc::new(ConversationBackend::default());
    let context = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        "invalid-http-url".to_owned(),
    )
    .with_direct_inference(backend.clone());
    let request = serde_json::from_value(json!({
        "model":"test", "input":[{"role":"user","content":"remember this"}],
    }))
    .unwrap();
    let response = crate::executor::compact_response(request, &context, None)
        .await
        .unwrap();
    let output = serde_json::to_value(response.output).unwrap();
    assert!(
        output
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "compaction" && item["encrypted_content"] == "done")
    );
    assert_eq!(response.usage.total_tokens, 5);
    assert_eq!(backend.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn native_steering_waits_for_a_completed_item_and_cancels_the_generation() {
    let backend = Arc::new(ConversationBackend::default());
    let context = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        "invalid-http-url".to_owned(),
    )
    .with_direct_inference(backend.clone());
    let (steering, _stream) = ResponseEventSink::channel(4096);
    steering.request_steering_boundary();
    let mut agent = AgentPipeline::new(request_context(), None, StreamRelay::detached(), None);
    agent.steering = Some(steering);
    let payload = fetch_stream_payload(
        &mut agent,
        &context,
        &ToolRegistry::default(),
        0,
        &ExecutorResponseBudget::new(),
    )
    .await
    .unwrap();
    assert_eq!(payload.status, "incomplete");
    assert_eq!(payload.incomplete_details.unwrap().reason.as_deref(), Some("steered"));
    assert_eq!(payload.output.len(), 1);
    assert_eq!(serde_json::to_value(&payload.output[0]).unwrap()["status"], "completed");
    assert!(
        payload.usage.is_none(),
        "unreceived terminal usage must not be fabricated"
    );
    assert!(backend.controls.lock().unwrap()[0].cancellation.is_cancelled());
}
