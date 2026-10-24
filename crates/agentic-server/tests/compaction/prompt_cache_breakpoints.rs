use super::*;
use serde_json::{Value, json};

#[tokio::test]
async fn breakpoints_follow_retained_parts_through_all_compaction_paths() {
    for path in ["standalone", "automatic", "trigger"] {
        let (model_url, captured, model) = spawn_sequential_model(vec![("checkpoint", 10), ("answer", 5)]).await;
        let pool = agentic_core::storage::create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .unwrap();
        let mut config = test_config(&model_url);
        config.responses.prompt_cache_options_supported = true;
        let mut state = test_state(&config);
        state.exec_ctx = Arc::new(
            agentic_core::executor::ExecutionContext::new(
                agentic_core::executor::ConversationHandler::new(agentic_core::storage::ConversationStore::new(
                    pool.clone(),
                )),
                agentic_core::executor::ResponseHandler::new(agentic_core::storage::ResponseStore::new(pool)),
                Arc::new(reqwest::Client::new()),
                model_url,
            )
            .with_responses_config(config.responses),
        );
        let (url, gateway) = spawn_gateway(state).await;
        let client = reqwest::Client::new();
        let parts = json!([
            {"type":"input_text","text":"retained prefix","prompt_cache_breakpoint":{"mode":"explicit"}},
            {"type":"input_image","image_url":"data:image/png;base64,AQ==","detail":"auto",
                "prompt_cache_breakpoint":{"mode":"explicit"}},
            {"type":"input_text","text":"unmarked suffix"}
        ]);
        let developer_parts = json!([{ "type":"input_text","text":"summarized developer context",
            "prompt_cache_breakpoint":{"mode":"explicit"}}]);
        let input = json!([
            {"role":"developer","content":developer_parts},
            {"role":"user","content":parts}
        ]);
        let options = json!({"mode":"explicit","ttl":"30m"});
        let mut request = json!({"model":"test-model","input":input,"store":false,"prompt_cache_options":options});
        if path == "standalone" {
            let response = client
                .post(format!("{url}/v1/responses/compact"))
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let compacted: Value = response.json().await.unwrap();
            assert_eq!(compacted["output"][0]["content"], parts);
            assert_eq!(compacted["output"][1]["type"], "compaction");
            // Replaying a compacted window must preserve the same boundaries.
            request["input"] = compacted["output"].clone();
        } else if path == "automatic" {
            request["context_management"] = json!([{"type":"compaction","compact_threshold":1}]);
        } else {
            request["store"] = json!(true);
            request["input"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"compaction_trigger"}));
        }
        let response = client
            .post(format!("{url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
        let body: Value = response.json().await.unwrap();
        if path == "trigger" {
            assert_eq!(body["output"][0]["type"], "compaction");
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"test-model","input":[],"store":false,
                    "previous_response_id":body["id"],"prompt_cache_options":options}))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        }
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 2, "{path}");
        // Summary inference receives the original boundaries, before content is replaced.
        assert_eq!(requests[0]["input"][0]["content"], developer_parts, "{path}");
        assert_eq!(requests[0]["input"][1]["content"], parts, "{path}");
        // The compacted model window retains user parts, then an unmarked summary.
        assert_eq!(requests[1]["input"].as_array().unwrap().len(), 2, "{path}");
        assert_eq!(requests[1]["input"][0]["content"], parts, "{path}");
        assert_eq!(requests[1]["input"][1]["role"], "assistant");
        assert_eq!(requests[1]["input"][1]["content"][0]["text"], "checkpoint");
        for request in requests.iter() {
            assert_eq!(request["prompt_cache_options"], options);
            // Boundaries are never copied onto the generated summary or summary instruction.
            let last = request["input"].as_array().unwrap().last().unwrap();
            assert!(!last.to_string().contains("prompt_cache_breakpoint"));
        }
        gateway.abort();
        model.abort();
    }
}
