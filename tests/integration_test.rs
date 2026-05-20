use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use http::StatusCode;
use tokio::net::TcpListener;

use agentic_api::handler::{self, AppState};
use agentic_api::store::ogx::OgxStore;

fn ogx_base_url() -> Option<String> {
    std::env::var("OGX_BASE_URL").ok()
}

fn start_gateway_with_ogx(vllm_port: u16, ogx_url: &str) -> (String, u16) {
    let ogx_url = ogx_url.to_owned();
    let rt = tokio::runtime::Handle::current();
    rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");

        let client = reqwest::Client::new();
        let ogx_store = Arc::new(OgxStore::new(&ogx_url, client.clone()));

        let state = Arc::new(AppState {
            vllm_base_url: format!("http://127.0.0.1:{vllm_port}"),
            openai_api_key: None,
            max_iterations: 10,
            client,
            response_store: ogx_store.clone(),
            vector_search: ogx_store,
        });

        let app = Router::new()
            .route("/v1/responses", post(handler::handle_responses))
            .route("/health", get(handler::health))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (addr, port)
    })
}

async fn spawn_capturing_vllm() -> (u16, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
    let captured = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let captured_clone = Arc::clone(&captured);

    let app = Router::new().route("/health", get(|| async { StatusCode::OK })).route(
        "/v1/responses",
        post(move |req: Request| {
            let captured = Arc::clone(&captured_clone);
            async move {
                let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
                captured.lock().unwrap().push(body);

                let resp = serde_json::json!({
                    "id": "resp_integration",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "integration test response"}]
                    }]
                });
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    serde_json::to_string(&resp).unwrap(),
                )
                    .into_response()
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, captured)
}

#[tokio::test]
async fn test_state_hydration_with_ogx() {
    let Some(ogx_url) = ogx_base_url() else {
        eprintln!("Skipping: OGX_BASE_URL not set");
        return;
    };

    let client = reqwest::Client::new();

    // Step 1: Create a response in OGx to establish conversation history
    let create_resp = client
        .post(format!("{ogx_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "test-model",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "What is Rust?"}]}],
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Rust is a systems programming language."}]
            }],
            "status": "completed",
            "store": true
        }))
        .send()
        .await
        .unwrap();

    assert!(
        create_resp.status().is_success(),
        "Failed to create response in OGx: {}",
        create_resp.text().await.unwrap_or_default()
    );

    let stored_response: serde_json::Value = create_resp.json().await.unwrap();
    let response_id = stored_response["id"].as_str().expect("response should have an id");
    eprintln!("Created OGx response: {response_id}");

    // Step 2: Start mock vLLM that captures requests
    let (vllm_port, captured) = spawn_capturing_vllm().await;

    // Step 3: Start gateway pointing at real OGx
    let (gw_addr, _) = start_gateway_with_ogx(vllm_port, &ogx_url);

    // Step 4: Send request with previous_response_id through the gateway
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "test-model",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Tell me more about its memory safety."}]}],
            "previous_response_id": response_id
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "Gateway should return 200");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_integration");

    // Step 5: Verify vLLM received hydrated input
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1, "vLLM should have received exactly one request");

    let vllm_request = &requests[0];
    let input = vllm_request["input"].as_array().expect("input should be an array");

    assert!(
        input.len() >= 3,
        "Expected at least 3 input items (previous user msg + assistant output + new user msg), got {}",
        input.len()
    );

    // previous_response_id should be stripped
    assert!(
        vllm_request.get("previous_response_id").is_none(),
        "previous_response_id should be stripped from the vLLM request"
    );

    eprintln!("Hydrated input has {} items", input.len());
    eprintln!("State hydration integration test passed!");
}
