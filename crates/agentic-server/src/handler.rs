use agentic_core::proxy::{ProxyBody, ProxyRequest, ProxyResponse, ProxyState, error_response};
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use tracing::warn;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

pub async fn health() -> impl IntoResponse {
    StatusCode::OK
}

pub async fn ready(State(state): State<ProxyState>) -> impl IntoResponse {
    let base = state.config.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = state.config.openai_api_key.as_deref() {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {trimmed}")) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .default_headers(headers)
        .build();

    let Ok(client) = client else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => StatusCode::OK,
        Ok(resp) => {
            warn!("LLM backend not ready: status {}", resp.status());
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(e) => {
            warn!("LLM backend unreachable: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn convert_response(resp: ProxyResponse) -> Response {
    let mut builder = Response::builder().status(resp.status);
    for (name, value) in &resp.headers {
        builder = builder.header(name, value);
    }
    match resp.body {
        ProxyBody::Full(bytes) => builder.body(Body::from(bytes)).expect("valid response"),
        ProxyBody::Stream(stream) => builder.body(Body::from_stream(stream)).expect("valid response"),
    }
}

pub async fn proxy_responses(State(state): State<ProxyState>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();

    let Ok(body_bytes) = axum::body::to_bytes(body, MAX_BODY_SIZE).await else {
        return convert_response(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "Request body too large",
        ));
    };

    let proxy_req = ProxyRequest {
        headers: parts.headers,
        body: body_bytes,
        query: parts.uri.query().map(String::from),
    };

    convert_response(agentic_core::proxy::proxy_request(proxy_req, &state).await)
}
