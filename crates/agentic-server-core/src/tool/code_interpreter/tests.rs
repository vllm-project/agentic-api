use super::*;

#[test]
fn normalizes_to_a_closed_function_contract() {
    let function = CodeInterpreterHandler::function_tool();
    assert_eq!(function.name, CODE_INTERPRETER_FUNCTION_NAME);
    assert_eq!(function.strict, Some(true));
    assert_eq!(function.parameters.expect("schema")["additionalProperties"], false);
}

struct FakeProvider;

impl CodeInterpreterProvider for FakeProvider {
    fn check_ready(&self) -> Result<(), ToolError> {
        Ok(())
    }

    fn max_concurrency(&self) -> std::num::NonZeroUsize {
        std::num::NonZeroUsize::new(1).expect("nonzero")
    }

    fn execute(
        &self,
        _code: String,
        _cancellation: Arc<ExecutionCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionOutput, ToolError>> + Send + '_>> {
        Box::pin(async {
            Ok(ExecutionOutput {
                status: ExecutionStatus::Completed,
                stdout: "x".repeat(128),
                stderr: String::new(),
            })
        })
    }
}

#[tokio::test]
async fn fake_provider_executes_through_handler_with_bounded_public_output() {
    let config = CodeInterpreterRuntimeConfig {
        max_stdout_bytes: std::num::NonZeroUsize::new(32).expect("nonzero"),
        ..CodeInterpreterRuntimeConfig::default()
    };
    let handler = CodeInterpreterExecutor::with_provider(config, Arc::new(FakeProvider)).expect("provider ready");
    let params: CodeInterpreterToolParam =
        serde_json::from_value(serde_json::json!({"container": {"type": "auto"}})).expect("valid declaration");
    let call = FunctionToolCall {
        async_execution: false,
        agent: Some(crate::types::io::AgentAttribution {
            agent_name: "/root/worker".to_owned(),
        }),
        id: "fc_123".to_owned(),
        call_id: "call_123".to_owned(),
        name: CODE_INTERPRETER_FUNCTION_NAME.to_owned(),
        namespace: None,
        arguments: r#"{"code":"print(42)"}"#.to_owned(),
        status: crate::types::event::MessageStatus::Completed,
    };
    let output = handler
        .execute(&call.call_id, &call.name, &call.arguments, &params)
        .await
        .expect("fake provider execution");
    let parsed: ExecutionOutput = serde_json::from_str(&output.output).expect("typed output");
    assert!(matches!(parsed.status, ExecutionStatus::Incomplete));
    assert!(parsed.stdout.len() <= config.max_stdout_bytes.get());
    assert!(parsed.stdout.contains("[output truncated]"));
    let OutputItem::CodeInterpreterCall(item) = handler
        .public_output(&call, &output, GatewayCallStatus::Completed, &params)
        .expect("public call item")
    else {
        panic!("expected code interpreter call");
    };
    assert_eq!(item.id, "ci_123");
    assert_eq!(
        item.agent.as_ref().map(|agent| agent.agent_name.as_str()),
        Some("/root/worker")
    );
    assert!(matches!(item.status, CodeInterpreterCallStatus::Incomplete));
    assert_eq!(item.outputs.expect("output").len(), 1);
}

#[test]
fn gateway_code_interpreter_ids_match_across_started_and_completed_items() {
    let config = CodeInterpreterRuntimeConfig::default();
    let mut public_ids = Vec::new();
    for (id, call_id, expected_suffix) in [
        ("fc_123", "call_456", Some("123")),
        ("fc_fc_123", "call_456", None),
        ("provider-item", "call_123", None),
        ("provider-item", "provider-call", None),
    ] {
        let call = FunctionToolCall {
            async_execution: false,
            agent: None,
            id: id.to_owned(),
            call_id: call_id.to_owned(),
            name: CODE_INTERPRETER_FUNCTION_NAME.to_owned(),
            namespace: None,
            arguments: r#"{"code":"print(42)"}"#.to_owned(),
            status: crate::types::event::MessageStatus::Completed,
        };
        let started_output = output_item(
            &call,
            config.max_source_bytes,
            CodeInterpreterCallStatus::InProgress,
            None,
        );
        assert!(
            started_output.to_input_item().is_none(),
            "gateway projection must not replay"
        );
        let stored = String::try_from(&crate::storage::InOutItem::Output(started_output.clone()))
            .expect("serialize gateway projection for history");
        let stored: serde_json::Value = serde_json::from_str(&stored).expect("stored gateway projection");
        assert_eq!(stored["_agentic_code_interpreter_origin"], "gateway");
        assert!(
            serde_json::to_value(&started_output)
                .expect("public projection serializes")
                .get("_agentic_code_interpreter_origin")
                .is_none()
        );
        let OutputItem::CodeInterpreterCall(started) = started_output else {
            panic!("expected code interpreter item");
        };
        let OutputItem::CodeInterpreterCall(completed) = output_item(
            &call,
            config.max_source_bytes,
            CodeInterpreterCallStatus::Completed,
            Some(vec![CodeInterpreterCallOutput::logs("42\n".to_owned())]),
        ) else {
            panic!("expected code interpreter item");
        };

        assert_eq!(started.id, completed.id);
        assert_eq!(started.container_id, completed.container_id);
        assert_eq!(started.code, completed.code);
        assert!(started.id.starts_with("ci_"));
        assert!(started.container_id.starts_with("cntr_"));
        assert!(!started.container_id.starts_with("cntr_fc_"));
        if let Some(suffix) = expected_suffix {
            assert_eq!(started.id, format!("ci_{suffix}"));
            assert_eq!(started.container_id, format!("cntr_{suffix}"));
        } else {
            assert!(started.id.starts_with("ci_h"));
            assert_eq!(
                started.id.strip_prefix("ci_"),
                completed.container_id.strip_prefix("cntr_")
            );
        }
        public_ids.push(started.id);
    }
    assert_ne!(public_ids[0], public_ids[1]);
    assert_ne!(public_ids[0], public_ids[2]);
}

struct RecordedInterpreter {
    jobs: std::sync::Mutex<std::collections::VecDeque<(String, ExecutionOutput)>>,
    started: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    executions: std::sync::atomic::AtomicUsize,
}

impl CodeInterpreterProvider for RecordedInterpreter {
    fn check_ready(&self) -> Result<(), ToolError> {
        Ok(())
    }
    fn max_concurrency(&self) -> std::num::NonZeroUsize {
        std::num::NonZeroUsize::new(1).unwrap()
    }
    fn execute(
        &self,
        code: String,
        cancellation: Arc<ExecutionCancellation>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecutionOutput, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let (expected, output) = self.jobs.lock().unwrap().pop_front().expect("recorded execution");
            assert_eq!(code, expected);
            let cancelled = Arc::clone(&self.cancelled);
            cancellation.install(move || {
                cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            if self.executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.started.add_permits(1);
                self.release.acquire().await.unwrap().forget();
            }
            Ok(output)
        })
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the recorded request, held tool, steering, and successor assertions in causal order"
)]
async fn recorded_hosted_steering_drains_real_interpreter_before_successor() {
    use crate::executor::session::SteeringQueue;
    use crate::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler, ResponseSession};
    use crate::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
    use crate::tool::GatewayExecutors;
    use crate::types::io::ToolChoice;
    use crate::types::{
        RequestPayload,
        websocket::{SteeringEvent, SteeringInput},
    };
    use crate::utils::common::deserialize_from_value;
    use axum::{Json, Router, routing::post};
    use futures::StreamExt;
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        fmt::Write as _,
        num::NonZeroUsize,
        sync::{Mutex, atomic::Ordering},
    };

    async fn next_event(stream: &mut crate::executor::BoxStream) -> Value {
        loop {
            let chunk = stream.next().await.expect("retained event");
            if let Some(value) = chunk.lines().find_map(|line| {
                line.strip_prefix("data: ")
                    .and_then(|data| serde_json::from_str(data).ok())
            }) {
                return value;
            }
        }
    }
    fn response_body(id: &str, output: &[Value]) -> String {
        let mut events = vec![json!({"type":"response.created", "response":{"id":id,"status":"in_progress"}})];
        for (index, item) in output.iter().enumerate() {
            let mut opening = item.clone();
            opening["status"] = json!("in_progress");
            if item["type"] == "function_call" {
                opening["arguments"] = json!("");
            } else {
                opening["content"] = json!([]);
            }
            events.push(json!({"type":"response.output_item.added","output_index":index,"item":opening}));
            events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        }
        events.push(json!({"type":"response.completed","response":{"id":id,"status":"completed","output":output}}));
        let mut body = String::new();
        for event in events {
            writeln!(body, "data: {event}\n").unwrap();
        }
        body
    }

    for capture_provider in ["openai-reference", "gateway"] {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/multi_turn_steering");
        let prefix = format!("multi-turn-steering-{capture_provider}-edge-tools-settings-");
        let paths: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix) && name.ends_with("-websocket.yaml"))
            })
            .collect();
        assert_eq!(
            paths.len(),
            1,
            "expected one {capture_provider} hosted capture: {paths:?}"
        );
        let recording: Value = serde_yaml::from_str(&std::fs::read_to_string(&paths[0]).unwrap()).unwrap();
        let session = recording["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["handshake"]["probe"]["case"] == "hosted-tool-running")
            .unwrap();
        let captured: Vec<Value> = session["frames"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|frame| frame["text"].as_str().and_then(|text| serde_json::from_str(text).ok()))
            .collect();
        let mut request = captured
            .iter()
            .find(|event| event["type"] == "response.create")
            .unwrap()
            .clone();
        request.as_object_mut().unwrap().remove("type");
        let request: RequestPayload = serde_json::from_value(request).unwrap();
        let update = &captured.iter().find(|event| event["type"] == "response.steer").unwrap()["input"];
        let steer: SteeringInput = serde_json::from_value(update.clone()).unwrap();
        let calls: Vec<_> = captured
            .iter()
            .filter(|event| {
                event["type"] == "response.output_item.done" && event["item"]["type"] == "code_interpreter_call"
            })
            .map(|event| event["item"].clone())
            .collect();
        let captured_status = calls[0]["status"].clone();
        let position = |kind| captured.iter().position(|event| event["type"] == kind).unwrap();
        assert!(position("response.code_interpreter_call.interpreting") < position("response.steer.accepted"));
        assert!(position("response.steer.accepted") < position("response.code_interpreter_call.completed"));
        assert!(position("response.code_interpreter_call.completed") < position("response.incomplete"));
        let jobs = calls
            .iter()
            .map(|call| {
                let status: ExecutionStatus = serde_json::from_value(call["status"].clone()).unwrap();
                let stdout = call["outputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|output| output["logs"].as_str().expect("recorded text output"))
                    .collect::<String>();
                (
                    call["code"].as_str().unwrap().to_owned(),
                    ExecutionOutput {
                        status,
                        stdout,
                        stderr: String::new(),
                    },
                )
            })
            .collect();
        let provider = Arc::new(RecordedInterpreter {
            jobs: Mutex::new(jobs),
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            cancelled: Arc::default(),
            executions: std::sync::atomic::AtomicUsize::default(),
        });
        let interpreter = CodeInterpreterExecutor::with_provider(
            crate::config::CodeInterpreterRuntimeConfig::default(),
            provider.clone(),
        )
        .unwrap();
        let mut bodies: VecDeque<_> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                response_body(
                    &format!("upstream_{index}"),
                    &[json!({"type":"function_call", "id":format!("fc_replay_{index}"),
                "call_id":format!("call_replay_{index}"), "name":"code_interpreter", "status":"completed",
                "arguments":json!({"code":call["code"]}).to_string()})],
                )
            })
            .collect();
        let final_response = captured
            .iter()
            .rev()
            .find(|event| event["type"] == "response.completed")
            .unwrap();
        let final_messages: Vec<_> = final_response["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "message")
            .cloned()
            .collect();
        bodies.push_back(response_body("upstream_final", &final_messages));
        let bodies = Arc::new(Mutex::new(bodies));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let route_requests = requests.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                route_requests.lock().unwrap().push(body);
                let response = bodies.lock().unwrap().pop_front().expect("recorded inference round");
                async move { ([("content-type", "text/event-stream")], response) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let mut exec = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(pool.clone())),
            ResponseHandler::new(ResponseStore::new(pool.clone())),
            Arc::new(reqwest::Client::new()),
            url,
        );
        exec.gateway_executors = GatewayExecutors::with_code_interpreter(interpreter);
        let exec = Arc::new(exec);
        let session = ResponseSession::new(NonZeroUsize::new(1024).unwrap(), NonZeroUsize::new(4_000_000).unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut parent = ExecuteRequest::new(request.clone(), exec.clone())
                .with_session(&session, None)
                .unwrap()
                .run_retained()
                .await
                .unwrap();
            let settings = parent.settings.clone();
            let mut observed = Vec::new();
            loop {
                let event = next_event(&mut parent.events).await;
                let interpreting = event["type"] == "response.code_interpreter_call.interpreting";
                observed.push(event);
                if interpreting {
                    break;
                }
            }
            provider.started.acquire().await.unwrap().forget();
            assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
            let mut queue = SteeringQueue::new(
                parent.response_id.clone(),
                NonZeroUsize::new(8).unwrap(),
                NonZeroUsize::new(100_000).unwrap(),
            )
            .unwrap();
            let accepted = queue.try_accept(steer).unwrap().clone();
            parent
                .sink
                .emit_steering(SteeringEvent::Accepted { steer: accepted })
                .await
                .unwrap();
            parent.sink.request_steering_boundary();
            assert!(!provider.cancelled.load(Ordering::SeqCst));
            assert_eq!(requests.lock().unwrap().len(), 1);
            provider.release.add_permits(1);
            loop {
                let event = next_event(&mut parent.events).await;
                let terminal = event["type"] == "response.incomplete";
                assert_ne!(event["type"], "response.failed", "{event}");
                assert_ne!(event["type"], "error", "{event}");
                observed.push(event);
                if terminal {
                    break;
                }
            }
            parent.owner.join().await.unwrap();
            let terminal = observed.last().unwrap();
            assert_eq!(terminal["response"]["incomplete_details"]["reason"], "steered");
            assert_eq!(terminal["response"]["output"][0]["status"], captured_status);
            let outputs = terminal["response"]["output"][0]["outputs"].as_array().unwrap();
            assert_eq!(
                outputs.as_slice(),
                calls[0]["outputs"].as_array().map_or(&[][..], Vec::as_slice)
            );
            let pin = session.pin_response(&parent.response_id).unwrap().unwrap();
            assert!(pin.required_steering_input().unwrap().is_empty());
            let mut successor_request = settings;
            successor_request.previous_response_id = Some(parent.response_id.clone());
            let prepared = ExecuteRequest::new(successor_request, exec.clone())
                .with_session(&session, Some(&pin))
                .unwrap()
                .with_steering(&queue)
                .unwrap()
                .prepare_retained()
                .await
                .unwrap();
            let reservation = queue
                .reserve_successor(prepared)
                .unwrap_or_else(|_| panic!("matching recorded batch"));
            let (mut successor, publication) = reservation.start_gated();
            successor.owner.activate().unwrap();
            let receipt = publication.wait_created().await.unwrap();
            assert_eq!(receipt.previous_response_id(), Some(parent.response_id.as_str()));
            assert!(publication.commit(&receipt).is_ok());
            loop {
                let event = next_event(&mut successor.events).await;
                let terminal = event["type"] == "response.completed";
                assert_ne!(event["type"], "response.failed", "{event}");
                assert_ne!(event["type"], "error", "{event}");
                observed.push(event);
                if terminal {
                    break;
                }
            }
            successor.owner.join().await.unwrap();
            let position = |kind| observed.iter().position(|event| event["type"] == kind).unwrap();
            assert!(position("response.code_interpreter_call.interpreting") < position("response.steer.accepted"));
            assert!(position("response.steer.accepted") < position("response.code_interpreter_call.completed"));
            assert!(position("response.code_interpreter_call.completed") < position("response.incomplete"));
            let successor_created = observed
                .iter()
                .position(|event| {
                    event["type"] == "response.created" && event["response"]["id"] == successor.response_id
                })
                .unwrap();
            assert!(position("response.incomplete") < successor_created);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), calls.len() + 1);
            assert_eq!(requests[0]["tools"], requests[1]["tools"]);
            assert_eq!(requests[0]["tool_choice"], requests[1]["tool_choice"]);
            assert!(
                requests[1]["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["role"] == "user" && item["content"] == *update)
            );
            let upstream_choice = deserialize_from_value::<Option<ToolChoice>>(requests[0]["tool_choice"].clone())
                .unwrap()
                .unwrap_or_default();
            assert_eq!(
                &upstream_choice,
                request.tool_choice.as_ref().unwrap_or(&ToolChoice::Auto)
            );
            assert_eq!(provider.executions.load(Ordering::SeqCst), calls.len());
            assert!(!provider.cancelled.load(Ordering::SeqCst));
        })
        .await
        .expect("recorded interpreter replay completes");
        server.abort();
        pool.close().await;
    }
}
