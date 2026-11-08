use std::time::Duration;

use agentic_core::config::ResponsesModelPolicy;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use super::*;

const ASTRA: &str = "gpt-6-astra";
const VLLM: &str = "vllm-fixture";
const CACHE_DISABLED: &str = "cache-disabled-fixture";

async fn configured_state(upstream_url: &str, global_cache_support: bool) -> StorageBackedState {
    let mut fixture = storage_backed_state(upstream_url).await;
    let config = &mut Arc::get_mut(&mut fixture.state.exec_ctx).unwrap().responses_config;
    config.prompt_cache_options_supported = global_cache_support;
    config.model_policies.insert(
        ASTRA.to_owned(),
        ResponsesModelPolicy {
            // Sampling, logprobs, and effort follow the public Astra model and migration guides.
            // The tier allowlist is specific to this test deployment.
            sampling_supported: Some(false),
            logprobs_supported: Some(false),
            reasoning_efforts: Some(["low", "medium", "high", "xhigh", "max"].map(str::to_owned).into()),
            service_tiers: Some(vec!["default".to_owned(), "priority".to_owned()]),
            prompt_cache_options_supported: Some(true),
            prompt_cache_retention_supported: Some(false),
        },
    );
    config.model_policies.insert(
        CACHE_DISABLED.to_owned(),
        ResponsesModelPolicy {
            prompt_cache_options_supported: Some(false),
            ..ResponsesModelPolicy::default()
        },
    );
    fixture
}

fn regional_rejection() -> Value {
    json!({"error": {
        "type": "invalid_request_error",
        "code": "unsupported_region",
        "param": "service_tier",
        "message": "Priority is unavailable in the configured region."
    }})
}

async fn capture_upstream() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| {
            let captured = Arc::clone(&captured);
            async move {
                let request: Value = serde_json::from_slice(&body).unwrap();
                let response = if request["service_tier"] == "priority" {
                    (StatusCode::BAD_REQUEST, axum::Json(regional_rejection())).into_response()
                } else if request["stream"] == true {
                    ([("content-type", "text/event-stream")], final_message_sse()).into_response()
                } else {
                    axum::Json(json!({"id":"resp_upstream","object":"response","created_at":0,
                        "model":request["model"],"status":"completed","service_tier":"default",
                        "output":[{"type":"message","id":"msg_upstream","role":"assistant","status":"completed",
                            "content":[{"type":"output_text","text":"checkpoint summary","annotations":[]}]}]}))
                    .into_response()
                };
                captured.lock().await.push(request);
                response
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), requests, upstream)
}

async fn post_json(client: &reqwest::Client, url: &str, request: &Value) -> reqwest::Response {
    client.post(url).json(request).send().await.unwrap()
}

async fn assert_local_error(response: reqwest::Response, param: &str, code: &str) {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error", "{error}");
    assert_eq!(error["error"]["param"], param, "{error}");
    assert_eq!(error["error"]["code"], code, "{error}");
}

#[tokio::test]
async fn http_applies_configured_restrictions_without_changing_other_models() {
    let (url, captured, upstream) = capture_upstream().await;
    let fixture = configured_state(&url, true).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let responses = format!("{url}/v1/responses");
    let client = reqwest::Client::new();
    let options = json!({"mode":"explicit","ttl":"30m"});
    let vllm_fields = json!({"temperature":0.7,"top_p":0.9,"top_logprobs":3,
        "include":["message.output_text.logprobs"],"reasoning":{"effort":"minimal"},
        "service_tier":"flex","prompt_cache_retention":"24h","cache_salt":"workspace"});

    for stream in [false, true] {
        for store in [false, true] {
            let mut request = json!({"model":ASTRA,"input":"hi","store":store,"stream":stream});
            for (param, fields) in [
                ("temperature", json!({"temperature":0.7})),
                ("top_p", json!({"top_p":0.9})),
                ("top_logprobs", json!({"top_logprobs":3})),
                (
                    "include",
                    json!({"include":["reasoning.encrypted_content","message.output_text.logprobs"]}),
                ),
                ("reasoning.effort", json!({"reasoning":{"effort":"none"}})),
                ("reasoning.effort", json!({"reasoning":{"effort":"minimal"}})),
                ("service_tier", json!({"service_tier":"flex"})),
                ("prompt_cache_retention", json!({"prompt_cache_retention":"24h"})),
            ] {
                let mut rejected = request.clone();
                rejected
                    .as_object_mut()
                    .unwrap()
                    .extend(fields.as_object().unwrap().clone());
                assert_local_error(
                    post_json(&client, &responses, &rejected).await,
                    param,
                    "unsupported_value",
                )
                .await;
            }
            request["model"] = json!(CACHE_DISABLED);
            request["prompt_cache_options"] = options.clone();
            assert_local_error(
                post_json(&client, &responses, &request).await,
                "prompt_cache_options",
                "unsupported_parameter",
            )
            .await;
        }
    }
    assert!(captured.lock().await.is_empty(), "local rejections reached upstream");

    for stream in [false, true] {
        for store in [false, true] {
            let mut request = json!({"model":VLLM,"input":"hi","store":store,"stream":stream});
            request
                .as_object_mut()
                .unwrap()
                .extend(vllm_fields.as_object().unwrap().clone());
            let response = post_json(&client, &responses, &request).await;
            assert_eq!(response.status(), StatusCode::OK);
            if stream {
                let events = response.text().await.unwrap();
                assert!(events.contains("response.completed"), "{events}");
            } else {
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["status"], "completed", "{body}");
            }
        }
    }
    let unset = json!({"model":ASTRA,"input":"hi","store":false,"temperature":null,"top_p":null,
        "top_logprobs":null,"reasoning":{"effort":null},"include":["reasoning.encrypted_content"]});
    let response = post_json(&client, &responses, &unset).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", response.text().await.unwrap());

    let requests = captured.lock().await;
    assert_eq!(requests.len(), 5);
    for request in &requests[..4] {
        assert_eq!(request["model"], VLLM);
        for (param, value) in vllm_fields.as_object().unwrap() {
            assert_eq!(request.get(param), Some(value), "{param}: {request}");
        }
    }
    assert_eq!(requests[4]["model"], ASTRA);
    gateway.abort();
    upstream.abort();
}

#[tokio::test]
async fn http_preserves_upstream_regional_errors_after_policy_admission() {
    let (url, captured, upstream) = capture_upstream().await;
    let fixture = configured_state(&url, false).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let responses = format!("{url}/v1/responses");
    let client = reqwest::Client::new();

    // The policy admits priority for Astra. Its configured upstream can still reject it for this region.
    for stream in [false, true] {
        let request = json!({"model":ASTRA,"input":"hi","store":false,"stream":stream,"service_tier":"priority"});
        let response = post_json(&client, &responses, &request).await;
        if stream && response.status() == StatusCode::OK {
            let events = response.text().await.unwrap();
            assert!(
                events.contains("Priority is unavailable in the configured region."),
                "{events}"
            );
            assert!(!events.contains("response.completed"), "{events}");
        } else {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.json::<Value>().await.unwrap(), regional_rejection());
        }
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 2, "upstream rejection must not cause a tier retry");
    for request in requests.iter() {
        assert_eq!(request["model"], ASTRA);
        assert_eq!(request["service_tier"], "priority");
    }
    gateway.abort();
    upstream.abort();
}

async fn websocket_result(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, request: &Value) -> Value {
    ws.send(Message::Text(request.to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::Text(text) = ws.next().await.unwrap().unwrap() {
                let event: Value = serde_json::from_str(&text).unwrap();
                if matches!(
                    event["type"].as_str(),
                    Some("error" | "response.completed" | "response.failed")
                ) {
                    return event;
                }
            }
        }
    })
    .await
    .expect("WebSocket terminal event")
}

#[tokio::test]
async fn websocket_policy_rejection_preserves_transient_parent_for_retry() {
    let (url, captured, upstream) = capture_upstream().await;
    let fixture = configured_state(&url, false).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let (mut ws, _) = connect_async(format!("{}/v1/responses", url.replacen("http://", "ws://", 1)))
        .await
        .unwrap();
    let options = json!({"mode":"explicit","ttl":"30m"});
    let parent = websocket_result(
        &mut ws,
        &json!({"type":"response.create","model":ASTRA,"input":"checkpoint context","store":false,
            "prompt_cache_options":options,"reasoning":{"effort":"low"}}),
    )
    .await;
    assert_eq!(parent["type"], "response.completed", "{parent}");
    let mut continuation = json!({"type":"response.create","model":ASTRA,"input":"follow-up","store":false,
        "previous_response_id":parent["response"]["id"]});
    for generate in [false, true] {
        let mut invalid = continuation.clone();
        invalid["generate"] = json!(generate);
        invalid["reasoning"] = json!({"effort":"none"});
        let error = websocket_result(&mut ws, &invalid).await;
        assert_eq!(error["type"], "error", "{error}");
        assert_eq!(error["status"], 400, "{error}");
        assert_eq!(error["error"]["param"], "reasoning.effort", "{error}");
        assert_eq!(error["error"]["code"], "unsupported_value", "{error}");
    }
    continuation["reasoning"] = json!({"effort":"high"});
    let retried = websocket_result(&mut ws, &continuation).await;
    assert_eq!(retried["type"], "response.completed", "{retried}");
    let regional_error = websocket_result(
        &mut ws,
        &json!({"type":"response.create","model":ASTRA,"input":"hi","store":false,"service_tier":"priority"}),
    )
    .await;
    assert_ne!(regional_error["type"], "response.completed");
    assert!(
        regional_error
            .to_string()
            .contains("Priority is unavailable in the configured region."),
        "{regional_error}"
    );
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["prompt_cache_options"], options);
    assert_eq!(requests[0]["reasoning"]["effort"], "low");
    assert_eq!(requests[1]["input"][0]["content"], "checkpoint context");
    assert_eq!(requests[1]["reasoning"]["effort"], "high");
    assert!(requests[1].get("prompt_cache_options").is_none());
    assert_eq!(requests[2]["service_tier"], "priority");
    gateway.abort();
    upstream.abort();
}

#[tokio::test]
async fn compact_uses_current_model_policy_with_stored_history() {
    let (url, captured, upstream) = capture_upstream().await;
    let fixture = configured_state(&url, false).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    let parent = post_json(
        &client,
        &format!("{url}/v1/responses"),
        &json!({"model":VLLM,"input":"stored checkpoint context","store":true}),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent: Value = parent.json().await.unwrap();
    let options = json!({"mode":"explicit","ttl":"30m"});
    let compact = format!("{url}/v1/responses/compact");
    let mut request = json!({"model":VLLM,"previous_response_id":parent["id"],"prompt_cache_options":options});
    assert_local_error(
        post_json(&client, &compact, &request).await,
        "prompt_cache_options",
        "unsupported_parameter",
    )
    .await;

    request["model"] = json!(ASTRA);
    let response = post_json(&client, &compact, &request).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], VLLM);
    assert_eq!(requests[1]["model"], ASTRA);
    assert_eq!(requests[1]["input"][0]["content"], "stored checkpoint context");
    assert_eq!(requests[1]["prompt_cache_options"], options);
    gateway.abort();
    upstream.abort();
}
