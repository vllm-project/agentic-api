use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::TryStreamExt;
use http::{HeaderMap, HeaderName, StatusCode};
use reqwest::Client;
use serde_json::Value;

use crate::config::RuntimeConfig;
use crate::error::Error;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

const REQUEST_DROP_EXTRA: &[&str] = &["host", "content-length"];

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

fn is_request_drop(name: &str) -> bool {
    is_hop_by_hop(name) || REQUEST_DROP_EXTRA.iter().any(|h| h.eq_ignore_ascii_case(name))
}

#[derive(Clone)]
pub struct ProxyState {
    pub config: RuntimeConfig,
    pub stream_client: Client,
    pub non_stream_client: Client,
}

impl ProxyState {
    /// # Errors
    ///
    /// Returns an error if the HTTP clients cannot be built (e.g. invalid TLS backend).
    pub fn new(config: RuntimeConfig) -> Result<Self, Error> {
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::HttpClient)?;

        let non_stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::HttpClient)?;

        Ok(Self {
            config,
            stream_client,
            non_stream_client,
        })
    }
}

fn filter_request_headers(headers: &HeaderMap, config: &RuntimeConfig) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if is_request_drop(name.as_str()) {
            continue;
        }
        if let Ok(n) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(v) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
                out.insert(n, v);
            }
        }
    }

    let has_auth = out.contains_key(reqwest::header::AUTHORIZATION);
    if !has_auth {
        if let Some(key) = config.openai_api_key.as_deref() {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {trimmed}")) {
                    out.insert(reqwest::header::AUTHORIZATION, v);
                }
            }
        }
    }

    out
}

fn filter_response_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        if let Ok(n) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(v) = http::HeaderValue::from_bytes(value.as_bytes()) {
                out.insert(n, v);
            }
        }
    }
    out
}

fn is_sse_content_type(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"))
}

fn proxy_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "api_error",
            "param": null,
            "code": code,
        }
    });
    (status, axum::Json(body)).into_response()
}

pub async fn proxy_responses(State(state): State<ProxyState>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body_bytes) = axum::body::to_bytes(body, MAX_BODY_SIZE).await else {
        return proxy_error(StatusCode::BAD_REQUEST, "body_too_large", "Request body too large");
    };

    let is_streaming = serde_json::from_slice::<Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("stream")?.as_bool())
        .unwrap_or(false);

    let vllm_headers = filter_request_headers(&parts.headers, &state.config);

    let base = state.config.llm_api_base.trim_end_matches('/');
    let mut url = format!("{base}/v1/responses");
    if let Some(q) = parts.uri.query() {
        url.push('?');
        url.push_str(q);
    }

    let client = if is_streaming {
        &state.stream_client
    } else {
        &state.non_stream_client
    };

    let vllm_resp = match client.post(&url).headers(vllm_headers).body(body_bytes).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return proxy_error(StatusCode::GATEWAY_TIMEOUT, "vllm_timeout", "vLLM timeout");
        }
        Err(_) => {
            return proxy_error(StatusCode::BAD_GATEWAY, "vllm_unavailable", "vLLM unavailable");
        }
    };

    let status = StatusCode::from_u16(vllm_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_headers = filter_response_headers(vllm_resp.headers());

    if is_sse_content_type(vllm_resp.headers()) {
        response_headers.insert("x-accel-buffering", http::HeaderValue::from_static("no"));

        let byte_stream = vllm_resp.bytes_stream().map_err(std::io::Error::other);

        let body = Body::from_stream(byte_stream);
        let mut resp = Response::new(body);
        *resp.status_mut() = status;
        *resp.headers_mut() = response_headers;
        return resp;
    }

    let payload: Bytes = match vllm_resp.bytes().await {
        Ok(b) => b,
        Err(_) => {
            return proxy_error(
                StatusCode::BAD_GATEWAY,
                "vllm_unavailable",
                "Failed to read vLLM response",
            );
        }
    };

    let mut resp = Response::new(Body::from(payload));
    *resp.status_mut() = status;
    *resp.headers_mut() = response_headers;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            llm_api_base: "http://localhost:8000".to_owned(),
            openai_api_key: Some("test-key".to_owned()),
            gateway_host: "127.0.0.1".to_owned(),
            gateway_port: 0,
            vllm_ready_timeout_s: 5.0,
            vllm_ready_interval_s: 0.1,
        }
    }

    fn test_config_no_key() -> RuntimeConfig {
        RuntimeConfig {
            openai_api_key: None,
            ..test_config()
        }
    }

    #[test]
    fn hop_by_hop_detected() {
        assert!(is_hop_by_hop("connection"));
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("keep-alive"));
        assert!(is_hop_by_hop("transfer-encoding"));
        assert!(is_hop_by_hop("proxy-authorization"));
    }

    #[test]
    fn non_hop_by_hop_passes() {
        assert!(!is_hop_by_hop("content-type"));
        assert!(!is_hop_by_hop("x-custom"));
        assert!(!is_hop_by_hop("authorization"));
    }

    #[test]
    fn request_drop_includes_host_and_content_length() {
        assert!(is_request_drop("host"));
        assert!(is_request_drop("content-length"));
        assert!(is_request_drop("connection"));
        assert!(!is_request_drop("content-type"));
    }

    #[test]
    fn filter_request_headers_strips_hop_by_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("proxy-authorization", "Basic abc".parse().unwrap());
        headers.insert("x-custom", "value".parse().unwrap());

        let config = test_config_no_key();
        let filtered = filter_request_headers(&headers, &config);

        assert!(filtered.contains_key("content-type"));
        assert!(filtered.contains_key("x-custom"));
        assert!(!filtered.contains_key("connection"));
        assert!(!filtered.contains_key("proxy-authorization"));
    }

    #[test]
    fn filter_request_headers_strips_host_and_content_length() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "example.com".parse().unwrap());
        headers.insert("content-length", "42".parse().unwrap());
        headers.insert("accept", "*/*".parse().unwrap());

        let config = test_config_no_key();
        let filtered = filter_request_headers(&headers, &config);

        assert!(!filtered.contains_key("host"));
        assert!(!filtered.contains_key("content-length"));
        assert!(filtered.contains_key("accept"));
    }

    #[test]
    fn auth_injected_when_no_client_auth() {
        let headers = HeaderMap::new();
        let config = test_config();
        let filtered = filter_request_headers(&headers, &config);

        assert_eq!(
            filtered.get("authorization").unwrap().to_str().unwrap(),
            "Bearer test-key"
        );
    }

    #[test]
    fn client_auth_takes_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let config = test_config();
        let filtered = filter_request_headers(&headers, &config);

        assert_eq!(
            filtered.get("authorization").unwrap().to_str().unwrap(),
            "Bearer client-token"
        );
    }

    #[test]
    fn no_auth_injected_when_key_empty() {
        let headers = HeaderMap::new();
        let config = RuntimeConfig {
            openai_api_key: Some("  ".to_owned()),
            ..test_config()
        };
        let filtered = filter_request_headers(&headers, &config);

        assert!(!filtered.contains_key("authorization"));
    }

    #[test]
    fn no_auth_injected_when_key_none() {
        let headers = HeaderMap::new();
        let config = test_config_no_key();
        let filtered = filter_request_headers(&headers, &config);

        assert!(!filtered.contains_key("authorization"));
    }

    #[test]
    fn filter_response_headers_strips_hop_by_hop() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("x-request-id", "abc".parse().unwrap());

        let filtered = filter_response_headers(&headers);

        assert!(filtered.contains_key("content-type"));
        assert!(filtered.contains_key("x-request-id"));
        assert!(!filtered.contains_key("connection"));
    }

    #[test]
    fn sse_content_type_detected() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "text/event-stream; charset=utf-8".parse().unwrap());
        assert!(is_sse_content_type(&headers));
    }

    #[test]
    fn sse_content_type_case_insensitive() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "Text/Event-Stream".parse().unwrap());
        assert!(is_sse_content_type(&headers));
    }

    #[test]
    fn non_sse_content_type_rejected() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        assert!(!is_sse_content_type(&headers));
    }

    #[test]
    fn missing_content_type_not_sse() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(!is_sse_content_type(&headers));
    }
}
