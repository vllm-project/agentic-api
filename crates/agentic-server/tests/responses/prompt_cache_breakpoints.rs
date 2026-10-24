//! Content-part boundaries must survive typed execution and durable history.
use super::*;
use serde_json::{Value, json};

fn marked_content() -> Value {
    json!([
        {"type":"input_text","text":"stable prefix","prompt_cache_breakpoint":{"mode":"explicit"}},
        {"type":"input_image","image_url":"data:image/png;base64,AQ==","detail":"auto",
            "prompt_cache_breakpoint":{"mode":"explicit"}},
        {"type":"input_text","text":"changing suffix"}
    ])
}

#[tokio::test]
async fn breakpoints_survive_json_sse_parity_and_durable_continuations() {
    for stream in [false, true] {
        let (url, captured, model) = if stream {
            spawn_tool_search_sse_sequence(
                (0..4)
                    .map(|i| final_message_sse().replace("msg_final", &format!("msg_{i}")))
                    .collect(),
            )
            .await
        } else {
            spawn_mock_vllm_json_capture().await
        };
        let mut fixture = storage_backed_state(&url).await;
        Arc::get_mut(&mut fixture.state.exec_ctx)
            .unwrap()
            .responses_config
            .prompt_cache_options_supported = true;
        let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
        let client = reqwest::Client::new();
        let parts = marked_content();
        let options = json!({"mode":"explicit","ttl":"30m"});
        let mut parent = Value::Null;
        for store in [false, true] {
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(
                    &json!({"model":"test","input":[{"type":"message","role":"user","content":parts}],
                    "store":store,"stream":stream,"prompt_cache_options":options}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.text().await.unwrap();
            let terminal: Value = if stream {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                    .find(|event| event["type"] == "response.completed")
                    .unwrap()["response"]
                    .clone()
            } else {
                serde_json::from_str(&body).unwrap()
            };
            if store {
                parent = terminal["id"].clone();
            }
        }
        // Restore the same marked items with and without repeating the top-level options.
        for repeat_options in [false, true] {
            let mut request = json!({"model":"test","input":"next question","store":true,
                "previous_response_id":parent,"stream":stream});
            if repeat_options {
                request["prompt_cache_options"] = options.clone();
            }
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&request)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "stream={stream}, repeated={repeat_options}: {body}"
            );
            if stream {
                assert!(body.contains("response.completed"), "{body}");
            }
        }
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 4);
        for request in requests.iter() {
            assert_eq!(request["input"][0]["content"], parts);
        }
        assert_eq!(requests[0]["input"], requests[1]["input"]);
        assert!(requests[2].get("prompt_cache_options").is_none());
        assert_eq!(requests[3]["prompt_cache_options"], options);
        gateway.abort();
        model.abort();
    }
}

#[tokio::test]
async fn breakpoint_presence_is_preserved_without_inserting_boundaries() {
    let (url, captured, model) = spawn_mock_vllm_json_capture().await;
    let mut fixture = storage_backed_state(&url).await;
    Arc::get_mut(&mut fixture.state.exec_ctx)
        .unwrap()
        .responses_config
        .prompt_cache_options_supported = true;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    // Breakpoint interpretation remains upstream-owned, including null/future values.
    for boundary in [None, Some(Value::Null), Some(json!({"mode":"future","extension":1}))] {
        let mut part = json!({"type":"input_text","text":"unmodified text"});
        if let Some(boundary) = boundary {
            part["prompt_cache_breakpoint"] = boundary;
        }
        for store in [false, true] {
            let response = reqwest::Client::new()
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"test","input":[{"role":"user","content":[part]}],
                    "store":store,"prompt_cache_options":{"mode":"explicit"}}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _: Value = response.json().await.unwrap();
        }
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 6);
    for pair in requests.as_chunks::<2>().0 {
        assert_eq!(pair[0]["input"][0]["content"], pair[1]["input"][0]["content"]);
        assert_eq!(pair[1]["prompt_cache_options"], json!({"mode":"explicit"}));
    }
    assert!(
        requests[1]["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert_eq!(
        requests[3]["input"][0]["content"][0].get("prompt_cache_breakpoint"),
        Some(&Value::Null)
    );
    assert_eq!(
        requests[5]["input"][0]["content"][0]["prompt_cache_breakpoint"],
        json!({"mode":"future","extension":1})
    );
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn unsupported_marked_parts_are_rejected_before_inference_and_storage() {
    let (url, captured, model) = spawn_mock_vllm_json_capture().await;
    let mut fixture = storage_backed_state(&url).await;
    Arc::get_mut(&mut fixture.state.exec_ctx)
        .unwrap()
        .responses_config
        .prompt_cache_options_supported = true;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    for part in [
        json!({"type":"input_file","file_id":"file_test","prompt_cache_breakpoint":{"mode":"explicit"}}),
        json!({"type":"future_part","prompt_cache_breakpoint":{"mode":"explicit"}}),
    ] {
        for store in [false, true] {
            let response = reqwest::Client::new()
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"test","input":[{"role":"user","content":[part]}],
                    "store":store,"prompt_cache_options":{"mode":"explicit"}}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if store { StatusCode::BAD_REQUEST } else { StatusCode::OK }
            );
            let _: Value = response.json().await.unwrap();
        }
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 2, "only direct requests reach the upstream");
    assert_eq!(requests[0]["input"][0]["content"][0]["type"], "input_file");
    assert_eq!(requests[1]["input"][0]["content"][0]["type"], "future_part");
    for request in requests.iter() {
        assert_eq!(
            request["input"][0]["content"][0]["prompt_cache_breakpoint"],
            json!({"mode":"explicit"})
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM responses")
            .fetch_one(fixture.pool.as_ref())
            .await
            .unwrap(),
        0
    );
    gateway.abort();
    model.abort();
}
