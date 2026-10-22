use super::*;
use serde_json::{Value, json};

fn enable_options(fixture: &mut StorageBackedState) {
    Arc::get_mut(&mut fixture.state.exec_ctx)
        .unwrap()
        .responses_config
        .prompt_cache_options_supported = true;
}

#[tokio::test]
async fn json_parity_and_durable_continuation_preserve_request_scoped_options() {
    let (url, captured, model) = spawn_mock_vllm_json_capture().await;
    let mut fixture = storage_backed_state(&url).await;
    enable_options(&mut fixture);
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    let mut parent = Value::Null;
    for options in [
        json!({}),
        json!({"mode":"implicit"}),
        json!({"ttl":"30m"}),
        json!({"mode":"explicit","ttl":"30m"}),
    ] {
        for store in [false, true] {
            let response = client
                .post(format!("{gateway_url}/v1/responses"))
                .json(&json!({"model":"test","input":"parent context","store":store,
                    "prompt_cache_options":options}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = response.json().await.unwrap();
            if store {
                parent = body["id"].clone();
            }
        }
    }
    for options in [None, Some(Value::Null), Some(json!({"mode":"implicit"}))] {
        let mut request = json!({"model":"test","input":"continue","store":true,"previous_response_id":parent});
        if let Some(options) = options {
            request["prompt_cache_options"] = options;
        }
        let response = client
            .post(format!("{gateway_url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _: Value = response.json().await.unwrap();
    }
    // Null is omitted for both store values; neither supplies defaults.
    for store in [false, true] {
        let response = client
            .post(format!("{gateway_url}/v1/responses"))
            .json(&json!({"model":"test","input":"hi","store":store,"prompt_cache_options":null}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _: Value = response.json().await.unwrap();
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 13);
    for (pair, options) in requests[..8].as_chunks::<2>().0.iter().zip([
        json!({}),
        json!({"mode":"implicit"}),
        json!({"ttl":"30m"}),
        json!({"mode":"explicit","ttl":"30m"}),
    ]) {
        assert_eq!(pair[0]["prompt_cache_options"], options);
        assert_eq!(pair[1]["prompt_cache_options"], options);
    }
    assert!(requests[8].get("prompt_cache_options").is_none());
    assert!(requests[9].get("prompt_cache_options").is_none());
    assert_eq!(requests[10]["prompt_cache_options"], json!({"mode":"implicit"}));
    for request in &requests[8..11] {
        assert_eq!(request["input"][0]["content"], "parent context");
        assert!(request.get("previous_response_id").is_none());
    }
    assert!(requests[11].get("prompt_cache_options").is_none());
    assert!(requests[12].get("prompt_cache_options").is_none());
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn sse_parity_preserves_explicit_mode_and_existing_content_breakpoint() {
    let (url, captured, model) = spawn_tool_search_sse_sequence(vec![final_message_sse(); 2]).await;
    let mut fixture = storage_backed_state(&url).await;
    enable_options(&mut fixture);
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let options = json!({"mode":"explicit","ttl":"30m"});
    let input = json!([{"type":"message","role":"developer","content":[{"type":"input_text","text":"stable prefix",
        "prompt_cache_breakpoint":{"mode":"explicit"}}]},{"type":"message","role":"user","content":"changing question"}]);
    for store in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("{gateway_url}/v1/responses"))
            .json(&json!({"model":"test","input":input,"stream":true,"store":store,"prompt_cache_options":options}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("response.completed"));
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request["prompt_cache_options"], options);
        assert_eq!(request["input"], input);
    }
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn invalid_and_unqualified_typed_options_fail_before_upstream_or_storage() {
    let (url, captured, model) = spawn_mock_vllm_json_capture().await;
    for supported in [false, true] {
        let mut fixture = storage_backed_state(&url).await;
        if supported {
            enable_options(&mut fixture);
        }
        let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
        for options in [
            json!({"mode":"auto"}),
            json!({"mode":null}),
            json!({"ttl":"24h"}),
            json!({"ttl":null}),
            json!({"prewarm":true}),
            json!({"comparison_response_id":"resp_1"}),
            json!([]),
        ] {
            let response = reqwest::Client::new()
                .post(format!("{gateway_url}/v1/responses"))
                .json(&json!({"model":"test","input":"hi","store":true,"prompt_cache_options":options}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        if !supported {
            for store in [false, true] {
                let request = json!({"model":"test","input":"hi","store":store,
                    "prompt_cache_options":{"mode":"explicit"},"prompt_cache_retention":"24h"});
                let response = reqwest::Client::new()
                    .post(format!("{gateway_url}/v1/responses"))
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["error"]["param"], "prompt_cache_options");
                assert_eq!(body["error"]["code"], "unsupported_parameter");
            }
        }
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM responses")
            .fetch_one(fixture.pool.as_ref())
            .await
            .unwrap();
        assert_eq!(count, 0);
        gateway.abort();
    }
    assert!(captured.lock().await.is_empty());
    model.abort();
}

#[tokio::test]
async fn review_combined_cache_controls_are_forwarded_independently() {
    let (url, captured, model) = spawn_mock_vllm_json_capture().await;
    let mut fixture = storage_backed_state(&url).await;
    enable_options(&mut fixture);
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let options = json!({"mode":"explicit","ttl":"30m"});
    for store in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("{gateway_url}/v1/responses"))
            .json(&json!({"model":"test","input":"hi","store":store,
                "prompt_cache_retention":"24h","prompt_cache_options":options}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _: Value = response.json().await.unwrap();
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request["prompt_cache_options"], options);
        assert_eq!(request["prompt_cache_retention"], "24h");
    }
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn stateless_cache_options_are_validated_before_upstream() {
    let values = [
        json!({"prewarm":true}),
        json!({"comparison_response_id":"resp_compare"}),
        json!({"future":{"enabled":true}}),
        json!({"mode":"future"}),
        json!({"mode":null}),
        json!([]),
        json!("opaque"),
        json!({}),
        Value::Null,
    ];
    for (stream, supported) in [(false, false), (true, false), (false, true), (true, true)] {
        let (url, captured, model) = if stream {
            spawn_tool_search_sse_sequence(vec![final_message_sse(); values.len()]).await
        } else {
            spawn_mock_vllm_json_capture().await
        };
        let mut fixture = storage_backed_state(&url).await;
        if supported {
            enable_options(&mut fixture);
        }
        let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
        for options in &values {
            let response = reqwest::Client::new()
                .post(format!("{gateway_url}/v1/responses"))
                .json(&json!({"model":"test","input":"hi","store":false,"stream":stream,
                    "prompt_cache_retention":"24h","prompt_cache_options":options}))
                .send()
                .await
                .unwrap();
            let accepted = options.is_null() || (supported && *options == json!({}));
            assert_eq!(
                response.status(),
                if accepted {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                },
                "{options}"
            );
            let body = response.text().await.unwrap();
            if stream && accepted {
                assert!(body.contains("response.completed"));
            }
        }
        let requests = captured.lock().await;
        assert_eq!(requests.len(), if supported { 2 } else { 1 });
        if supported {
            assert_eq!(requests[0]["prompt_cache_options"], json!({}));
        }
        assert!(requests.last().unwrap().get("prompt_cache_options").is_none());
        assert!(
            requests
                .iter()
                .all(|request| request["prompt_cache_retention"] == "24h")
        );
        gateway.abort();
        model.abort();
    }
}

#[tokio::test]
async fn review_json_and_stored_responses_echo_the_current_request_options() {
    let (url, _captured, model) = spawn_mock_vllm_json_capture().await;
    let mut fixture = storage_backed_state(&url).await;
    enable_options(&mut fixture);
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    let mut parent = Value::Null;
    for options in [
        Some(json!({"mode":"explicit","ttl":"30m"})),
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"mode":"implicit"})),
    ] {
        let mut request = json!({"model":"test","input":"hi","store":true});
        if !parent.is_null() {
            request["previous_response_id"] = parent;
        }
        if let Some(options) = &options {
            request["prompt_cache_options"] = options.clone();
        }
        let response = client
            .post(format!("{gateway_url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        let expected = options.unwrap_or(Value::Null);
        assert_eq!(body.get("prompt_cache_options"), Some(&expected));
        parent = body["id"].clone();
        let stored: Value = client
            .get(format!("{gateway_url}/v1/responses/{}", parent.as_str().unwrap()))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stored.get("prompt_cache_options"), Some(&expected));
    }
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn review_sse_options_are_consistent_from_created_to_completed_and_storage() {
    let mut upstream_events: Vec<Value> = final_message_sse()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let mut in_progress = upstream_events[0].clone();
    in_progress["type"] = json!("response.in_progress");
    upstream_events.insert(1, in_progress);
    let upstream = encode_sse_events(upstream_events)
        .lines()
        .map(|line| {
            let Some(data) = line.strip_prefix("data: ") else {
                return line.to_owned();
            };
            let Ok(mut event) = serde_json::from_str::<Value>(data) else {
                return line.to_owned();
            };
            if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
                response.insert(
                    "prompt_cache_options".into(),
                    json!({"mode":"implicit","prewarm":false}),
                );
            }
            format!("data: {event}")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n\n";
    // Each stored response must have independent output item identifiers.
    let second = upstream
        .replace("msg_final", "msg_second")
        .replace("up_final", "up_second");
    let (url, _captured, model) = spawn_tool_search_sse_sequence(vec![upstream, second]).await;
    let mut fixture = storage_backed_state(&url).await;
    enable_options(&mut fixture);
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    for options in [Some(json!({"mode":"explicit","ttl":"30m"})), None] {
        let mut request = json!({"model":"test","input":"hi","store":true,"stream":true});
        if let Some(options) = &options {
            request["prompt_cache_options"] = options.clone();
        }
        let response = client
            .post(format!("{gateway_url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        let events: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        let expected = options.unwrap_or(Value::Null);
        for kind in ["response.created", "response.in_progress", "response.completed"] {
            let event = events
                .iter()
                .find(|event| event["type"] == kind)
                .unwrap_or_else(|| panic!("missing {kind}: {events:?}"));
            assert_eq!(event["response"].get("prompt_cache_options"), Some(&expected), "{kind}");
        }
        let id = events.last().unwrap()["response"]["id"].as_str().unwrap();
        let stored: Value = client
            .get(format!("{gateway_url}/v1/responses/{id}"))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stored.get("prompt_cache_options"), Some(&expected));
    }
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn stateless_cache_options_reject_duplicate_fields() {
    let (url, captured, model) = spawn_mock_vllm_json_capture_bytes().await;
    let fixture = storage_backed_state(&url).await;
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let wire = r#"{ "model":"test", "input":"hi", "store":false,
        "prompt_cache_options": { "prewarm":true, "mode":"explicit", "mode":"implicit" } }"#;
    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/responses"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(wire)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(captured.lock().await.is_empty());
    gateway.abort();
    model.abort();
}
