//! Root inference tier selection across collaboration, streaming, and persistence.
use super::*;

async fn tier_model(
    status: &'static str,
    tier: Option<&'static str>,
    cache_options: bool,
) -> (Arc<ExecutionContext>, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(request): Json<Value>| async move {
            assert_eq!(request["service_tier"], "priority");
            if cache_options {
                assert_eq!(request["prompt_cache_options"], json!({"mode":"explicit","ttl":"30m"}));
                let prefix = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["role"] == "user")
                    .unwrap();
                assert_eq!(prefix["content"], marked_prefix());
                assert_eq!(request["prompt_cache_retention"], "24h");
            } else {
                assert_eq!(request["prompt_cache_retention"], "24h");
            }
            let input = request["input"].as_array().unwrap();
            let child = input.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("You are `/root/worker`");
            let final_root = !child
                && input
                    .iter()
                    .any(|item| item["call_id"] == "spawn_worker" && item["type"] == "function_call_output");
            let output = if child {
                vec![message("child finished")]
            } else if final_root {
                if status == "completed" {
                    vec![message("root finished")]
                } else {
                    vec![]
                }
            } else {
                vec![function(
                    "spawn_worker",
                    "spawn_agent",
                    json!({"task_name":"worker","message":"do work","fork_turns":"all"}),
                )]
            };
            let mut payload = json!({"id":"upstream","object":"response","created_at":0,"model":"test",
            "status":if final_root {status} else {"completed"},"output":output,
            "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}});
            if let Some(value) = if child {
                Some("flex")
            } else if final_root {
                tier
            } else {
                Some("auto")
            } {
                payload["service_tier"] = json!(value);
            }
            if final_root && status == "incomplete" {
                payload["incomplete_details"] = json!({"reason":"max_output_tokens"});
            }
            if final_root && status == "failed" {
                payload["error"] = json!({"code":"server_error","message":"upstream failed"});
            }
            if request["stream"] == true {
                let event_type = match payload["status"].as_str().unwrap() {
                    "failed" => "response.failed",
                    "incomplete" => "response.incomplete",
                    _ => "response.completed",
                };
                let events = upstream_events(&payload)
                    .replace("\"type\":\"response.completed\"", &format!("\"type\":\"{event_type}\""));
                ([("content-type", "text/event-stream")], events).into_response()
            } else {
                Json(payload).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let exec = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(pool.clone())),
        ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        format!("http://{address}"),
    );
    let exec = exec.with_responses_config(crate::config::ResponsesConfig {
        prompt_cache_options_supported: cache_options,
        ..crate::config::ResponsesConfig::default()
    });
    (Arc::new(exec), server)
}

#[tokio::test]
async fn final_root_tier_controls_json_sse_and_stored_snapshot() {
    for stream in [false, true] {
        for status in ["completed", "failed", "incomplete"] {
            for (tier, cache_options) in [
                (Some("default"), false),
                (None, false),
                (Some("default"), true),
                (None, true),
            ] {
                let (exec, server) = tier_model(status, tier, cache_options).await;
                let request = RequestPayload {
                    model: "test".into(),
                    store: true,
                    stream,
                    parallel_tool_calls: Some(true),
                    temperature: Some(0.4),
                    metadata: Some(json!({"review": "multi-agent"})),
                    service_tier: Some("priority".into()),
                    prompt_cache_retention: Some(crate::types::request_response::PromptCacheRetention::TwentyFourHours),
                    prompt_cache_options: cache_options
                        .then(|| serde_json::from_value(json!({"mode":"explicit","ttl":"30m"})).unwrap()),
                    input: if cache_options {
                        serde_json::from_value(json!([{"role":"user","content":marked_prefix()}])).unwrap()
                    } else {
                        ResponsesInput::Text("review context".into())
                    },
                    multi_agent: Some(MultiAgentConfig {
                        enabled: true,
                        max_concurrent_subagents: Some(1),
                    }),
                    ..Default::default()
                };
                let payload = match ExecuteRequest::new(request, exec.clone()).run().await.unwrap() {
                    Either::Left(payload) => payload,
                    Either::Right(mut events) => {
                        let mut terminal = None;
                        while let Some(chunk) = events.next().await {
                            for data in chunk.lines().filter_map(|line| line.strip_prefix("data: ")) {
                                if data == "[DONE]" {
                                    continue;
                                }
                                let event: Value = serde_json::from_str(data).unwrap();
                                assert_ne!(event["type"], "error", "{event}");
                                if matches!(
                                    event["type"].as_str(),
                                    Some("response.completed" | "response.failed" | "response.incomplete")
                                ) {
                                    terminal = Some(
                                        serde_json::from_value::<ResponsePayload>(event["response"].clone()).unwrap(),
                                    );
                                }
                            }
                        }
                        terminal.unwrap()
                    }
                };
                let serialized_tier = if stream && tier.is_none() {
                    Some("default")
                } else {
                    tier
                };
                assert_eq!(
                    payload.service_tier.as_deref(),
                    serialized_tier,
                    "{status}, stream={stream}"
                );
                assert_eq!(payload.status, if status == "failed" { "error" } else { status });
                let expected_options =
                    cache_options.then(|| serde_json::from_value(json!({"mode":"explicit","ttl":"30m"})).unwrap());
                assert_eq!(payload.standard_fields.prompt_cache_options, expected_options);
                assert!(payload.standard_fields.store);
                assert!(payload.standard_fields.parallel_tool_calls);
                assert!((payload.standard_fields.temperature - 0.4).abs() < f64::EPSILON);
                assert_eq!(payload.standard_fields.metadata["review"], "multi-agent");
                assert_eq!(payload.standard_fields.completed_at.is_some(), status == "completed");
                if status == "failed" {
                    assert!(matches!(
                        exec.resp_handler.retrieve(&payload.id).await,
                        Err(ExecutorError::Storage(crate::StorageError::NotFound { .. }))
                    ));
                } else {
                    let stored = exec.resp_handler.retrieve(&payload.id).await.unwrap();
                    assert_eq!(stored.standard_fields.prompt_cache_options, expected_options);
                    assert_eq!(stored.service_tier.as_deref(), tier.or(Some("default")));
                }
                server.abort();
            }
        }
    }
}

fn marked_prefix() -> Value {
    json!([
        {"type":"input_text","text":"review context","prompt_cache_breakpoint":{"mode":"explicit"}},
        {"type":"input_image","image_url":"data:image/png;base64,AQ==","detail":"auto",
            "prompt_cache_breakpoint":{"mode":"explicit"}},
        {"type":"input_text","text":"unmarked suffix"}
    ])
}
