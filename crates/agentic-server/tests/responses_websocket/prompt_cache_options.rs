use super::*;

#[tokio::test]
async fn options_are_request_scoped_on_transient_websocket_continuations() {
    let mock = MockResponsesServer::start(
        (0..4)
            .map(|i| sse_response(&format!("resp_{i}"), &format!("msg_{i}"), "ok"))
            .collect(),
    )
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
    let mut parent = Value::Null;
    for (i, options) in [
        Some(json!({"mode":"explicit","ttl":"30m"})),
        None,
        Some(Value::Null),
        Some(json!({"mode":"implicit"})),
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = json!({"type":"response.create","model":"test-model","input":"hi","store":false,"prompt_cache_retention":"24h"});
        if i > 0 {
            request["previous_response_id"] = parent;
        }
        let expected = options.clone().unwrap_or(Value::Null);
        if let Some(options) = options {
            request["prompt_cache_options"] = options;
        }
        send_json(&mut ws, request).await;
        let events = recv_until_completed(&mut ws).await;
        for response in events.iter().filter_map(|event| event.get("response")) {
            assert_eq!(response.get("prompt_cache_options"), Some(&expected));
        }
        parent = events.last().unwrap()["response"]["id"].clone();
    }
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request["prompt_cache_retention"] == "24h")
    );
    assert_eq!(
        requests[0]["prompt_cache_options"],
        json!({"mode":"explicit","ttl":"30m"})
    );
    assert!(requests[1].get("prompt_cache_options").is_none());
    assert!(requests[2].get("prompt_cache_options").is_none());
    assert_eq!(requests[3]["prompt_cache_options"], json!({"mode":"implicit"}));
    assert!(requests[3]["input"].as_array().unwrap().len() > 1);
    gateway.abort();
}

#[tokio::test]
async fn unsupported_options_reject_generating_and_local_completion_before_history_lookup() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&url).await;
    for generate in [true, false] {
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","input":"hi",
            "previous_response_id":"resp_missing","generate":generate,"store":false,
            "prompt_cache_options":{"mode":"explicit"}}),
        )
        .await;
        let error = recv_json(&mut ws).await;
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["param"], "prompt_cache_options");
        assert_eq!(error["error"]["code"], "unsupported_parameter");
    }
    assert!(mock.request_bodies().await.is_empty());
    gateway.abort();
}

#[tokio::test]
async fn invalid_options_reject_before_websocket_inference() {
    let mock = MockResponsesServer::start(vec![]).await;
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
    for options in [
        json!([]),
        json!({"mode":"auto"}),
        json!({"mode":null}),
        json!({"ttl":"24h"}),
        json!({"prewarm":false}),
        json!({"comparison_response_id":"resp_diagnostics"}),
    ] {
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","input":"hi",
            "prompt_cache_options":options}),
        )
        .await;
        let error = recv_json(&mut ws).await;
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "invalid_request_error");
    }
    assert!(mock.request_bodies().await.is_empty());
    gateway.abort();
}

#[tokio::test]
async fn local_completion_echoes_options_without_inference() {
    let mock = MockResponsesServer::start(vec![]).await;
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
    let options = json!({"mode":"explicit","ttl":"30m"});
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"hi",
        "store":false,"generate":false,"prompt_cache_options":options,"prompt_cache_retention":"24h"}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    for response in events.iter().filter_map(|event| event.get("response")) {
        assert_eq!(response.get("prompt_cache_options"), Some(&options));
    }
    assert!(mock.request_bodies().await.is_empty());
    gateway.abort();
}
