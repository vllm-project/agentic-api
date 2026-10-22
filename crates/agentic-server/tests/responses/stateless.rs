use super::*;
use serde_json::{Value, json};

#[tokio::test]
async fn custom_tool_round_trip_uses_client_history_without_storage() {
    for stream in [false, true] {
        let arguments = r#"{"input":"hello"}"#;
        let (llm_url, requests, llm) = if stream {
            spawn_tool_search_sse_sequence(vec![
                function_call_sse("echo", "fc_echo", "call_echo", arguments),
                final_message_sse(),
            ])
            .await
        } else {
            spawn_mock_vllm_json_capture_sequence(vec![
                json!({
                    "id":"up_call", "object":"response", "created_at":0,
                    "model":"test", "status":"completed", "output":[{
                        "type":"function_call", "id":"fc_echo", "call_id":"call_echo",
                        "name":"echo", "arguments":arguments, "status":"completed"
                    }]
                }),
                json!({
                    "id":"up_final", "object":"response", "created_at":0,
                    "model":"test", "status":"completed", "output":[{
                        "type":"message", "id":"msg_final", "role":"assistant", "status":"completed",
                        "content":[{"type":"output_text", "text":"done"}]
                    }]
                }),
            ])
            .await
        };
        // Enabled storage proves that store:false bypasses writes even when a DB is available.
        let fixture = storage_backed_state(&llm_url).await;
        let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
        let client = reqwest::Client::new();
        let tools = json!([{"type":"custom", "name":"echo", "description":"Echo raw text."}]);
        let kwargs = json!({"enable_thinking": false, "nested": {"values": [1, null, "kept"]}});
        let mut input = vec![json!({"role":"user", "content":"echo hello"})];
        let mut response_ids = Vec::new();
        for turn in 0..2 {
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"test", "store":false, "stream":stream, "tools":tools,
                    "input":input, "chat_template_kwargs":kwargs, "top_k":20, "x_provider_option":null}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = if stream {
                let events = sse_events(&response.text().await.unwrap());
                let terminal = events.last().unwrap();
                assert_eq!(terminal["type"], "response.completed", "{events:?}");
                terminal["response"].clone()
            } else {
                response.json().await.unwrap()
            };
            assert_eq!(body["store"], false);
            let id = body["id"].as_str().unwrap();
            assert!(id.starts_with("resp_"));
            assert_response_not_persisted(&fixture.pool, id).await;
            response_ids.push(id.to_owned());
            if turn == 0 {
                assert_eq!(body["output"][0]["type"], "custom_tool_call");
                assert_eq!(body["output"][0]["name"], "echo");
                assert_eq!(body["output"][0]["input"], "hello");
                input.extend(body["output"].as_array().unwrap().iter().cloned());
                input.push(json!({"type":"custom_tool_call_output", "call_id":"call_echo", "output":"hello"}));
            } else {
                assert_eq!(body["output"][0]["type"], "message");
            }
        }
        assert_ne!(response_ids[0], response_ids[1]);
        assert_eq!(stored_row_counts(&fixture.pool).await, (0, 0));
        let captured = requests.lock().await;
        assert_eq!(captured.len(), 2);
        for request in captured.iter() {
            assert_eq!(request["chat_template_kwargs"], kwargs);
            assert_eq!(request["top_k"], 20);
            assert_eq!(request.get("x_provider_option"), Some(&Value::Null));
            assert_eq!(request["store"], false);
            assert!(request.get("previous_response_id").is_none());
            assert!(request.get("conversation").is_none());
            assert_eq!(request["tools"][0]["type"], "function");
            assert_eq!(
                request["tools"][0]["parameters"]["properties"]["input"]["type"],
                "string"
            );
        }
        let replay = captured[1]["input"].as_array().unwrap();
        assert_eq!(replay.len(), 3, "only the client-supplied history is used");
        assert_eq!(replay[0]["type"], "message");
        assert_eq!(replay[1]["type"], "function_call");
        assert_eq!(replay[1]["call_id"], "call_echo");
        assert_eq!(
            serde_json::from_str::<Value>(replay[1]["arguments"].as_str().unwrap()).unwrap(),
            json!({"input":"hello"})
        );
        assert_eq!(replay[2]["type"], "function_call_output");
        assert_eq!(replay[2]["call_id"], "call_echo");
        assert_eq!(replay[2]["output"], "hello");
        gateway.abort();
        llm.abort();
    }
}

#[tokio::test]
async fn store_false_missing_state_ids_fail_without_falling_back_to_client_history() {
    let (llm_url, requests, llm) = spawn_mock_vllm_json_capture().await;
    let fixture = storage_backed_state(&llm_url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    for stream in [false, true] {
        for state in [
            json!({"previous_response_id":"resp_missing"}),
            json!({"conversation":"conv_missing"}),
        ] {
            let mut request = json!({"model":"test", "input":"complete history", "store":false, "stream":stream});
            request
                .as_object_mut()
                .unwrap()
                .extend(state.as_object().unwrap().clone());
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&request)
                .send()
                .await
                .unwrap();
            let expected = if state.get("previous_response_id").is_some() {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::NOT_FOUND
            };
            assert_eq!(response.status(), expected, "{request}");
        }
    }
    assert!(requests.lock().await.is_empty());
    assert_eq!(stored_row_counts(&fixture.pool).await, (0, 0));
    gateway.abort();
    llm.abort();
}
