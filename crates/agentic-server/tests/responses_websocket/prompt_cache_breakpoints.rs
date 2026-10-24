use super::*;

#[tokio::test]
async fn breakpoints_survive_transient_and_reconnected_durable_history() {
    for store in [false, true] {
        let mock = MockResponsesServer::start(vec![
            sse_response("resp_parent", "msg_parent", "parent answer"),
            sse_response("resp_next", "msg_next", "next answer"),
            sse_response("resp_last", "msg_last", "last answer"),
        ])
        .await;
        let fixture = storage_backed_state_with_responses_config(
            &mock.url,
            ResponsesConfig {
                prompt_cache_options_supported: true,
                ..ResponsesConfig::default()
            },
        )
        .await;
        let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&url).await;
        let parts = json!([
            {"type":"input_text","text":"prefix","prompt_cache_breakpoint":{"mode":"explicit"}},
            {"type":"input_image","image_url":"data:image/png;base64,AQ==","detail":"auto",
                "prompt_cache_breakpoint":{"mode":"explicit"}},
            {"type":"input_text","text":"suffix"}
        ]);
        let options = json!({"mode":"explicit","ttl":"30m"});
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model",
            "input":[{"role":"user","content":parts}],"store":store,"prompt_cache_options":options}),
        )
        .await;
        let events = recv_until_completed(&mut ws).await;
        let mut parent = events.last().unwrap()["response"]["id"].clone();
        // A fresh connection forces the durable path to restore from storage.
        if store {
            ws.close(None).await.unwrap();
            ws = connect_responses_ws(&url).await;
        }
        for repeat_options in [false, true] {
            let mut request = json!({"type":"response.create","model":"test-model","input":"next",
                "previous_response_id":parent,"store":store});
            if repeat_options {
                request["prompt_cache_options"] = options.clone();
            }
            send_json(&mut ws, request).await;
            let events = recv_until_completed(&mut ws).await;
            assert!(
                events.iter().all(|event| event["type"] != "error"),
                "store={store}: {events:?}"
            );
            parent = events.last().unwrap()["response"]["id"].clone();
        }
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request["input"][0]["content"], parts);
        }
        assert_eq!(requests[0]["prompt_cache_options"], options);
        assert!(requests[1].get("prompt_cache_options").is_none());
        assert_eq!(requests[2]["prompt_cache_options"], options);
        gateway.abort();
    }
}

#[tokio::test]
async fn locally_completed_history_preserves_content_breakpoints() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_final", "msg_final", "ok")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&url).await;
    let parts = json!([{ "type":"input_text","text":"local prefix",
        "prompt_cache_breakpoint":{"mode":"explicit"}}]);
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","generate":false,
        "store":false,"input":[{"role":"user","content":parts}]}),
    )
    .await;
    let parent = recv_until_completed(&mut ws).await.last().unwrap()["response"]["id"].clone();
    assert!(mock.request_bodies().await.is_empty());
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","store":false,
        "previous_response_id":parent,"input":"next"}),
    )
    .await;
    recv_until_completed(&mut ws).await;
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["input"][0]["content"], parts);
    assert!(requests[0].get("prompt_cache_options").is_none());
    gateway.abort();
}
