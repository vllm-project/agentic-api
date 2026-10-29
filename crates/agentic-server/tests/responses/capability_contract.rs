//! Gateway transport tests. The upstream policies below are synthetic, not provider recordings.
use super::*;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn restricted_cases() -> Vec<(&'static str, Value)> {
    vec![
        ("temperature", json!({"temperature":0.7})),
        ("top_p", json!({"top_p":0.9})),
        ("top_logprobs", json!({"top_logprobs":3})),
        ("include", json!({"include":["message.output_text.logprobs"]})),
        ("reasoning", json!({"reasoning":{"effort":"none"}})),
        ("reasoning", json!({"reasoning":{"effort":"minimal"}})),
        ("service_tier", json!({"service_tier":"priority"})),
    ]
}

async fn capability_model() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| {
            let captured = Arc::clone(&captured);
            async move {
                let request: Value = serde_json::from_slice(&body).unwrap();
                captured.lock().await.push(request.clone());
                if request["model"] == "restricted-model-fixture" {
                    let param = restricted_cases()
                        .into_iter()
                        .find_map(|(param, fields)| (request.get(param) == fields.get(param)).then_some(param));
                    if let Some(param) = param {
                        return (
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({"error":{
                                "type":"invalid_request_error", "code":"unsupported_value", "param":param,
                                "message":format!("{param} is unavailable for this model in the configured region")
                            }})),
                        )
                            .into_response();
                    }
                }
                if request["stream"] == true {
                    return ([("content-type", "text/event-stream")], final_message_sse()).into_response();
                }
                axum::Json(json!({"id":"upstream","object":"response","created_at":0,
                "model":request["model"],"status":"completed","output":[],"service_tier":"default"}))
                .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), requests, handle)
}

#[tokio::test]
async fn capability_contract_http_preserves_controls_and_model_specific_rejections() {
    let (url, captured, model) = capability_model().await;
    let fixture = storage_backed_state(&url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    for fields in [
        json!({}),
        json!({"reasoning":{"effort":"low"}}),
        json!({"temperature":null,"top_p":null}),
    ] {
        let mut request = json!({"model":"restricted-model-fixture","input":"hi","store":false});
        request
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let response = client
            .post(format!("{url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _: Value = response.json().await.unwrap();
    }
    for stream in [false, true] {
        for store in [false, true] {
            for model_name in ["restricted-model-fixture", "vllm-fixture"] {
                for (param, fields) in restricted_cases() {
                    let mut request = json!({"model":model_name,"input":"hi","store":store,"stream":stream});
                    request
                        .as_object_mut()
                        .unwrap()
                        .extend(fields.as_object().unwrap().clone());
                    let response = client
                        .post(format!("{url}/v1/responses"))
                        .json(&request)
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = response.text().await.unwrap();
                    if model_name == "restricted-model-fixture" {
                        assert!(
                            status == StatusCode::BAD_REQUEST || (stream && status == StatusCode::OK),
                            "{body}"
                        );
                        assert!(body.contains(param) && body.contains("configured region"), "{body}");
                        assert!(!body.contains("response.completed"), "{body}");
                        if !stream {
                            let error: Value = serde_json::from_str(&body).unwrap();
                            assert_eq!(error["error"]["param"], param);
                            assert_eq!(error["error"]["code"], "unsupported_value");
                        }
                    } else {
                        assert_eq!(status, StatusCode::OK, "{body}");
                        if stream {
                            assert!(body.contains("response.completed"), "{body}");
                        }
                    }
                    let actual = captured.lock().await.last().unwrap().clone();
                    assert_eq!(actual["model"], model_name);
                    assert_eq!(
                        actual.get(param),
                        fields.get(param),
                        "silently changed {param}: {actual}"
                    );
                }
            }
        }
    }
    gateway.abort();
    model.abort();
}

#[tokio::test]
async fn capability_contract_websocket_keeps_model_specific_errors_and_connection_usable() {
    let (url, captured, model) = capability_model().await;
    let fixture = storage_backed_state(&url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let (mut ws, _) = connect_async(format!("{}/v1/responses", url.replacen("http://", "ws://", 1)))
        .await
        .unwrap();
    for (param, fields) in restricted_cases() {
        for model_name in ["restricted-model-fixture", "vllm-fixture"] {
            let mut request = json!({"type":"response.create","model":model_name,"input":"hi","store":false});
            request
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            ws.send(Message::Text(request.to_string().into())).await.unwrap();
            let terminal = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let message = ws.next().await.unwrap().unwrap();
                    if let Message::Text(text) = message {
                        let event: Value = serde_json::from_str(&text).unwrap();
                        if matches!(
                            event["type"].as_str(),
                            Some("error" | "response.completed" | "response.failed")
                        ) {
                            break event;
                        }
                    }
                }
            })
            .await
            .unwrap();
            if model_name == "restricted-model-fixture" {
                assert_ne!(terminal["type"], "response.completed", "{terminal}");
                let text = terminal.to_string();
                assert!(text.contains(param) && text.contains("configured region"), "{text}");
            } else {
                assert_eq!(terminal["type"], "response.completed", "{terminal}");
            }
            let actual = captured.lock().await.last().unwrap().clone();
            assert_eq!(actual.get(param), fields.get(param));
        }
    }
    gateway.abort();
    model.abort();
}
