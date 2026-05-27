use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use reqwest::Client;
use serde_json::Value;
use tracing::warn;

use crate::config::Config;
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

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

fn is_request_drop(name: &str) -> bool {
    is_hop_by_hop(name) || REQUEST_DROP_EXTRA.iter().any(|h| h.eq_ignore_ascii_case(name))
}

pub struct ProxyRequest {
    pub headers: HeaderMap,
    pub body: Bytes,
    pub query: Option<String>,
}

pub enum ProxyBody {
    Full(Bytes),
    Stream(Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>),
}

pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: ProxyBody,
}

#[derive(Clone)]
pub struct ProxyState {
    pub config: Config,
    pub stream_client: Client,
    pub non_stream_client: Client,
}

impl ProxyState {
    /// # Errors
    ///
    /// Returns an error if the HTTP clients cannot be built.
    pub fn new(config: Config) -> Result<Self, Error> {
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(900))
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

fn filter_request_headers(headers: &HeaderMap, config: &Config) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if is_request_drop(name.as_str()) {
            continue;
        }
        if let Ok(n) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(v) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
                out.append(n, v);
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
            if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
                out.append(n, v);
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

#[must_use]
pub fn error_response(status: StatusCode, code: &str, message: &str) -> ProxyResponse {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "api_error",
            "param": null,
            "code": code,
        }
    });
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    ProxyResponse {
        status,
        headers,
        body: ProxyBody::Full(Bytes::from(serde_json::to_vec(&body).unwrap_or_default())),
    }
}

pub async fn proxy_request(request: ProxyRequest, state: &ProxyState) -> ProxyResponse {
    let is_streaming = serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|v| v.get("stream")?.as_bool())
        .unwrap_or(false);

    let llm_headers = filter_request_headers(&request.headers, &state.config);

    let base = state.config.llm_api_base.trim_end_matches('/');
    let mut url = format!("{base}/v1/responses");
    if let Some(q) = &request.query {
        url.push('?');
        url.push_str(q);
    }

    let client = if is_streaming {
        &state.stream_client
    } else {
        &state.non_stream_client
    };

    let llm_resp = match client.post(&url).headers(llm_headers).body(request.body).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            warn!("LLM request timed out: {e}");
            return error_response(StatusCode::GATEWAY_TIMEOUT, "llm_timeout", "LLM timeout");
        }
        Err(e) => {
            warn!("LLM request failed: {e}");
            return error_response(StatusCode::BAD_GATEWAY, "llm_unavailable", "LLM unavailable");
        }
    };

    let status = StatusCode::from_u16(llm_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_headers = filter_response_headers(llm_resp.headers());

    if is_sse_content_type(llm_resp.headers()) {
        response_headers.insert("x-accel-buffering", HeaderValue::from_static("no"));

        let byte_stream = llm_resp.bytes_stream().map_err(std::io::Error::other);

        return ProxyResponse {
            status,
            headers: response_headers,
            body: ProxyBody::Stream(Box::pin(byte_stream)),
        };
    }

    let payload: Bytes = match llm_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!("failed to read LLM response body: {e}");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "llm_unavailable",
                "Failed to read LLM response",
            );
        }
    };

    ProxyResponse {
        status,
        headers: response_headers,
        body: ProxyBody::Full(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_config() -> Config {
        Config {
            llm_api_base: "http://localhost:8000".to_owned(),
            openai_api_key: Some("test-key".to_owned()),
            llm_ready_timeout_s: 5.0,
            llm_ready_interval_s: 0.1,
        }
    }

    fn test_config_no_key() -> Config {
        Config {
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
        let config = Config {
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
