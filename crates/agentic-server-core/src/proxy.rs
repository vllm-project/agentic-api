use std::collections::HashSet;
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

const REQUEST_DROP_EXTRA: &[&str] = &["host", "content-length", "accept-encoding"];
const PROCESSED_RESPONSE_DROP_EXTRA: &[&str] = &["content-length", "content-encoding"];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

fn is_request_drop(name: &str) -> bool {
    is_hop_by_hop(name) || REQUEST_DROP_EXTRA.iter().any(|h| h.eq_ignore_ascii_case(name))
}

fn connection_options(headers: &HeaderMap) -> HashSet<HeaderName> {
    let mut options = HashSet::new();
    for value in headers.get_all("connection") {
        for option in value.as_bytes().split(|byte| *byte == b',') {
            if let Ok(name) = HeaderName::from_bytes(option.trim_ascii()) {
                options.insert(name);
            }
        }
    }
    options
}

/// Raw request data forwarded to the default Responses upstream endpoint.
pub struct ProxyRequest {
    pub headers: HeaderMap,
    pub body: Bytes,
    pub query: Option<String>,
}

impl ProxyRequest {
    /// Read only the transport preference while preserving the original request bytes.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        serde_json::from_slice::<Value>(&self.body)
            .ok()
            .and_then(|value| value.get("stream")?.as_bool())
            .unwrap_or(false)
    }
}

/// Authentication fallback used when proxying a request to an upstream API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyAuth {
    OpenAiBearer,
    Anthropic,
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

/// Build the request headers forwarded to an upstream API.
///
/// Hop-by-hop and origin-specific headers are removed, all other headers stay
/// open-ended, and the configured credential is injected only when the client
/// did not supply one. W3C trace headers are replaced with the current gateway
/// span's context; caller headers are never forwarded as the outbound parent.
#[must_use]
pub fn upstream_request_headers(headers: &HeaderMap, config: &Config, auth: ProxyAuth) -> reqwest::header::HeaderMap {
    let connection_options = connection_options(headers);
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if is_request_drop(name.as_str()) || connection_options.contains(name) {
            continue;
        }
        if let Ok(n) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
            && let Ok(v) = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
        {
            out.append(n, v);
        }
    }

    let has_auth = out.contains_key(reqwest::header::AUTHORIZATION);
    let has_api_key = out.contains_key("x-api-key");
    if !has_auth
        && !has_api_key
        && let Some(key) = config.openai_api_key.as_deref()
    {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            let (name, value) = match auth {
                ProxyAuth::OpenAiBearer => (reqwest::header::AUTHORIZATION, format!("Bearer {trimmed}")),
                ProxyAuth::Anthropic => (
                    reqwest::header::HeaderName::from_static("x-api-key"),
                    trimmed.to_owned(),
                ),
            };
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&value) {
                out.insert(name, v);
            }
        }
    }

    crate::executor::telemetry::stages::inject_context(&mut out);
    out
}

fn filter_response_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let connection_options = connection_options(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop(name.as_str()) || connection_options.contains(name) {
            continue;
        }
        if let Ok(n) = HeaderName::from_bytes(name.as_str().as_bytes())
            && let Ok(v) = HeaderValue::from_bytes(value.as_bytes())
        {
            out.append(n, v);
        }
    }
    out
}

/// Build response headers for an upstream body consumed or transformed in-process.
///
/// Hop-by-hop headers are removed along with representation metadata that may no
/// longer describe the emitted body. Request IDs, retry guidance, and rate-limit
/// metadata remain open-ended.
#[must_use]
pub fn processed_response_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = filter_response_headers(headers);
    for name in PROCESSED_RESPONSE_DROP_EXTRA {
        out.remove(*name);
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
    error_response_for_auth(status, code, message, ProxyAuth::OpenAiBearer)
}

#[must_use]
pub fn error_response_for_auth(status: StatusCode, code: &str, message: &str, auth: ProxyAuth) -> ProxyResponse {
    let body = match auth {
        ProxyAuth::OpenAiBearer => serde_json::json!({
            "error": {
                "message": message,
                "type": "api_error",
                "param": null,
                "code": code,
            }
        }),
        ProxyAuth::Anthropic => serde_json::json!({
            "type": "error",
            "error": {
                "type": if status == StatusCode::BAD_REQUEST { "invalid_request_error" } else { "api_error" },
                "message": message,
            }
        }),
    };
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    ProxyResponse {
        status,
        headers,
        body: ProxyBody::Full(Bytes::from(serde_json::to_vec(&body).unwrap_or_default())),
    }
}

/// Proxy a GET request to an arbitrary upstream path.
///
/// Applies the same header filtering and auth injection as [`proxy_request`].
/// Uses the non-streaming client; the response body is returned as a full
/// [`ProxyBody::Full`] payload.
pub async fn proxy_get(path: &str, request_headers: &HeaderMap, state: &ProxyState) -> ProxyResponse {
    let llm_headers = upstream_request_headers(request_headers, &state.config, ProxyAuth::OpenAiBearer);
    let base = state.config.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/{}", path.trim_start_matches('/'));

    let llm_resp = match state.non_stream_client.get(&url).headers(llm_headers).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            warn!("upstream GET {path} timed out: {e}");
            return error_response(StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", "upstream timeout");
        }
        Err(e) => {
            warn!("upstream GET {path} failed: {e}");
            return error_response(StatusCode::BAD_GATEWAY, "upstream_unavailable", "upstream unavailable");
        }
    };

    let status = StatusCode::from_u16(llm_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = filter_response_headers(llm_resp.headers());

    match llm_resp.bytes().await {
        Ok(payload) => ProxyResponse {
            status,
            headers: response_headers,
            body: ProxyBody::Full(payload),
        },
        Err(e) => {
            warn!("failed to read upstream GET {path} body: {e}");
            error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unavailable",
                "failed to read upstream response",
            )
        }
    }
}

/// Proxy a request to the default `/v1/responses` upstream endpoint.
pub async fn proxy_request(request: ProxyRequest, state: &ProxyState) -> ProxyResponse {
    proxy_request_with_path(request, "/v1/responses", ProxyAuth::OpenAiBearer, state).await
}

/// Proxy a raw request to a selected upstream path.
pub async fn proxy_request_with_path(
    request: ProxyRequest,
    path: &str,
    auth: ProxyAuth,
    state: &ProxyState,
) -> ProxyResponse {
    let is_streaming = request.is_streaming();

    let llm_headers = upstream_request_headers(&request.headers, &state.config, auth);

    let base = state.config.llm_api_base.trim_end_matches('/');
    let mut url = format!("{base}/{}", path.trim_start_matches('/'));
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
            return error_response_for_auth(StatusCode::GATEWAY_TIMEOUT, "llm_timeout", "LLM timeout", auth);
        }
        Err(e) => {
            warn!("LLM request failed: {e}");
            return error_response_for_auth(StatusCode::BAD_GATEWAY, "llm_unavailable", "LLM unavailable", auth);
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
            return error_response_for_auth(
                StatusCode::BAD_GATEWAY,
                "llm_unavailable",
                "Failed to read LLM response",
                auth,
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
            skip_llm_ready_check: false,
            db_url: None,
            postgres: crate::config::PostgresConfig::default(),
            sqlite: crate::config::SqliteConfig::default(),
            tools: crate::config::ToolRuntimeConfig::default(),
            responses: crate::config::ResponsesConfig::default(),
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
        assert!(is_request_drop("accept-encoding"));
        assert!(is_request_drop("connection"));
        assert!(!is_request_drop("content-type"));
    }

    #[test]
    fn proxy_request_retains_legacy_construction_shape() {
        let _request = ProxyRequest {
            headers: HeaderMap::new(),
            body: Bytes::new(),
            query: None,
        };
    }

    #[test]
    fn filter_request_headers_strips_hop_by_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("proxy-authorization", "Basic abc".parse().unwrap());
        headers.insert("x-custom", "value".parse().unwrap());

        let config = test_config_no_key();
        let filtered = upstream_request_headers(&headers, &config, ProxyAuth::OpenAiBearer);

        assert!(filtered.contains_key("content-type"));
        assert!(filtered.contains_key("x-custom"));
        assert!(!filtered.contains_key("connection"));
        assert!(!filtered.contains_key("proxy-authorization"));
    }

    #[test]
    fn filter_request_headers_strips_connection_options() {
        let mut headers = HeaderMap::new();
        headers.append(
            "connection",
            HeaderValue::from_bytes(b"keep-alive, \tX-Request-Hop\t").unwrap(),
        );
        headers.append("connection", "x-repeated-hop".parse().unwrap());
        headers.insert("x-request-hop", "first".parse().unwrap());
        headers.insert("x-repeated-hop", "second".parse().unwrap());
        headers.insert("x-end-to-end", "preserved".parse().unwrap());

        let filtered = upstream_request_headers(&headers, &test_config_no_key(), ProxyAuth::OpenAiBearer);

        assert!(!filtered.contains_key("connection"));
        assert!(!filtered.contains_key("x-request-hop"));
        assert!(!filtered.contains_key("x-repeated-hop"));
        assert_eq!(filtered["x-end-to-end"], "preserved");
    }

    #[test]
    fn filter_request_headers_strips_host_and_content_length() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "example.com".parse().unwrap());
        headers.insert("content-length", "42".parse().unwrap());
        headers.insert("accept", "*/*".parse().unwrap());

        let config = test_config_no_key();
        let filtered = upstream_request_headers(&headers, &config, ProxyAuth::OpenAiBearer);

        assert!(!filtered.contains_key("host"));
        assert!(!filtered.contains_key("content-length"));
        assert!(filtered.contains_key("accept"));
    }

    #[test]
    fn auth_injected_when_no_client_auth() {
        let headers = HeaderMap::new();
        let config = test_config();
        let filtered = upstream_request_headers(&headers, &config, ProxyAuth::OpenAiBearer);

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
        let filtered = upstream_request_headers(&headers, &config, ProxyAuth::OpenAiBearer);

        assert_eq!(
            filtered.get("authorization").unwrap().to_str().unwrap(),
            "Bearer client-token"
        );
    }

    #[test]
    fn anthropic_auth_preserves_client_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "client-anthropic-key".parse().unwrap());

        let filtered = upstream_request_headers(&headers, &test_config(), ProxyAuth::Anthropic);

        assert_eq!(filtered.get("x-api-key").unwrap(), "client-anthropic-key");
        assert!(!filtered.contains_key("authorization"));
    }

    #[test]
    fn anthropic_auth_uses_configured_key_as_api_key_fallback() {
        let filtered = upstream_request_headers(&HeaderMap::new(), &test_config(), ProxyAuth::Anthropic);

        assert_eq!(filtered.get("x-api-key").unwrap(), "test-key");
        assert!(!filtered.contains_key("authorization"));
    }

    #[test]
    fn no_auth_injected_when_key_empty() {
        let headers = HeaderMap::new();
        let config = Config {
            openai_api_key: Some("  ".to_owned()),
            ..test_config()
        };
        let filtered = upstream_request_headers(&headers, &config, ProxyAuth::OpenAiBearer);

        assert!(!filtered.contains_key("authorization"));
    }

    #[test]
    fn no_auth_injected_when_key_none() {
        let headers = HeaderMap::new();
        let config = test_config_no_key();
        let filtered = upstream_request_headers(&headers, &config, ProxyAuth::OpenAiBearer);

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
    fn filter_response_headers_strips_connection_options() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            "connection",
            reqwest::header::HeaderValue::from_bytes(b"upgrade, \tX-Response-Hop\t").unwrap(),
        );
        headers.append("connection", "x-repeated-hop".parse().unwrap());
        headers.insert("x-response-hop", "first".parse().unwrap());
        headers.insert("x-repeated-hop", "second".parse().unwrap());
        headers.insert("x-end-to-end", "preserved".parse().unwrap());

        let filtered = filter_response_headers(&headers);

        assert!(!filtered.contains_key("connection"));
        assert!(!filtered.contains_key("x-response-hop"));
        assert!(!filtered.contains_key("x-repeated-hop"));
        assert_eq!(filtered["x-end-to-end"], "preserved");
    }

    #[test]
    fn processed_response_headers_preserve_metadata_and_strip_representation_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("request-id", "req_123".parse().unwrap());
        headers.insert("retry-after", "3".parse().unwrap());
        headers.insert("anthropic-ratelimit-requests-remaining", "7".parse().unwrap());
        headers.insert("content-length", "99".parse().unwrap());
        headers.insert("content-encoding", "gzip".parse().unwrap());

        let filtered = processed_response_headers(&headers);

        assert_eq!(filtered["request-id"], "req_123");
        assert_eq!(filtered["retry-after"], "3");
        assert_eq!(filtered["anthropic-ratelimit-requests-remaining"], "7");
        assert!(!filtered.contains_key("content-length"));
        assert!(!filtered.contains_key("content-encoding"));
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
