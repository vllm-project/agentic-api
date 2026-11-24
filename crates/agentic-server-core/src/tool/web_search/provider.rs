//! Shared search-provider contract, normalized result types, the credential
//! and endpoint cleanup every provider is built with, and the response helpers
//! every provider module reads through.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;

use futures::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::args::WebSearchArguments;
use crate::config::WebSearchProviderKind;
use crate::tool::handler::{MAX_GATEWAY_TOOL_OUTPUT_BYTES, ToolError};
use crate::types::tools::WebSearchToolParam;

/// Provider credential whose `Debug` output never contains the secret.
#[derive(Clone)]
pub(crate) struct ApiKey(pub String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Trims a configured base URL and its trailing slashes; blank counts as unset.
pub(crate) fn clean_base_url(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// A provider's credential and endpoint, cleaned from environment-style values.
#[derive(Debug, Clone)]
pub(crate) struct ProviderSettings {
    pub(crate) api_key: Option<ApiKey>,
    pub(crate) base_url: Option<String>,
}

impl ProviderSettings {
    /// A blank key counts as unset, so a provider that needs one fails at
    /// execution time. A blank base URL falls back to the provider's public
    /// endpoint, and stays unset for a provider without one.
    pub(crate) fn from_values(kind: WebSearchProviderKind, api_key: Option<String>, base_url: Option<String>) -> Self {
        let api_key = api_key
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(ApiKey);
        let base_url = base_url
            .and_then(|value| clean_base_url(&value))
            .or_else(|| kind.default_base_url().map(str::to_owned));
        Self { api_key, base_url }
    }
}

/// A status a provider answers when the account's balance or plan is used up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BillingStatus {
    pub(crate) code: u16,
    /// What the status means, read as "<provider> reported <meaning>".
    pub(crate) meaning: &'static str,
}

/// Maps a non-2xx provider response to an actionable, credential-free error.
///
/// `401`/`403` name the key variable without echoing the upstream body; `429`
/// is reported without retrying (a retry loop would blur the gateway tool
/// timeout) and carries the upstream `Retry-After` (or Brave's
/// `X-RateLimit-Reset`) so the caller can back off; the provider's
/// `billing_statuses` are reported without retrying. Any other status carries
/// the upstream body.
pub(super) async fn failure_from_status(
    resp: reqwest::Response,
    kind: WebSearchProviderKind,
    billing_statuses: &[BillingStatus],
) -> ToolError {
    let status = resp.status();
    let name = kind.display_name();
    if let Some(billing) = billing_statuses.iter().find(|billing| billing.code == status.as_u16()) {
        return ToolError::Execution(format!(
            "{name} reported {} ({}); the gateway does not retry",
            billing.meaning, billing.code
        ));
    }
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            // Keenable also serves requests without a key, so the key may not be the cause.
            let rejected = if matches!(kind, WebSearchProviderKind::Keenable) {
                "request"
            } else {
                "API key"
            };
            ToolError::Execution(format!(
                "{name} rejected the {rejected} ({status}); check {}",
                kind.default_api_key_env()
            ))
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let headers: &[&str] = if matches!(kind, WebSearchProviderKind::Brave) {
                &["retry-after", "x-ratelimit-reset"]
            } else {
                &["retry-after"]
            };
            let retry_after = headers
                .iter()
                .find_map(|name| resp.headers().get(*name).and_then(|value| value.to_str().ok()))
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let hint = retry_after.map_or_else(
                || "; no Retry-After header was provided".to_owned(),
                |value| format!("; retry after {value}"),
            );
            ToolError::Execution(format!(
                "{name} rate limited the request ({status}); the gateway does not retry{hint}"
            ))
        }
        _ => {
            let body = read_response_limited(resp, kind).await.unwrap_or_default();
            // "Brave Search" already names the search.
            let source = if matches!(kind, WebSearchProviderKind::Brave) {
                name.to_owned()
            } else {
                format!("{name} search")
            };
            ToolError::Execution(format!("{source} returned {status}: {body}"))
        }
    }
}

/// A search backend behind `web_search`.
///
/// Implementations shape one provider request per query and normalize the
/// response into [`WebSearchProviderResponse`]; the handler owns fan-out,
/// concurrency, and the model-facing output shape.
pub(crate) trait WebSearchProvider: std::fmt::Debug + Send + Sync {
    fn search<'a>(
        &'a self,
        query: &'a str,
        args: &'a WebSearchArguments,
        config: &'a WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>>;

    /// Provider-imposed ceiling on concurrent requests, if any. The handler
    /// never schedules more queries at once than this allows.
    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        None
    }
}

/// One normalized search hit. Serialized field names are the model-facing
/// contract and reuse You.com's wire names; `url` is empty when the provider
/// omitted it, and empty/`None` fields are not serialized. Fields outside this
/// struct (cosmetic `thumbnail_url` / `favicon_url`, unknown keys) are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WebSearchResult {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<String>,
    /// Kept as `page_age` (You.com's wire name) so existing model-facing output
    /// is unchanged; a provider-neutral name is deferred to the first provider
    /// that needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_age: Option<String>,
    /// Live-crawled page body, present when the provider fetched the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<WebSearchPageContents>,
}

/// Live-crawled page body in the formats the provider returned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WebSearchPageContents {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<String>,
}

/// Per-query provider metadata echoed to the model as `metadata[]`.
///
/// `provider` is serialized for every backend except the default You.com, so
/// existing consumers and recordings keep the exact You.com shape (`query`,
/// `search_uuid`, `latency`) while alternative providers are visible to the
/// model (#291 Q5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WebSearchProviderMetadata {
    #[serde(skip_serializing_if = "WebSearchProviderKind::is_you")]
    pub provider: WebSearchProviderKind,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_uuid: Option<String>,
    /// Provider-reported latency in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<f64>,
}

/// Normalized response for a single query.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WebSearchProviderResponse {
    pub web: Vec<WebSearchResult>,
    pub news: Vec<WebSearchResult>,
    pub metadata: WebSearchProviderMetadata,
}

/// Deserializes an explicit JSON `null` as the field's default instead of
/// failing, so a degenerate provider response cannot fail the whole search.
pub(crate) use crate::utils::common::null_as_default;

/// Reads a provider HTTP response body, failing as soon as it exceeds
/// [`MAX_GATEWAY_TOOL_OUTPUT_BYTES`] so an oversized provider reply is never
/// buffered in full. Every provider module reads its responses through here.
pub(super) async fn read_response_limited(
    resp: reqwest::Response,
    provider: WebSearchProviderKind,
) -> Result<String, ToolError> {
    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| ToolError::Execution(format!("failed to read {provider} search response: {error}")))?;
        if chunk.len() > MAX_GATEWAY_TOOL_OUTPUT_BYTES.saturating_sub(body.len()) {
            return Err(ToolError::Execution(format!(
                "{provider} search response exceeded {MAX_GATEWAY_TOOL_OUTPUT_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| ToolError::Execution(format!("{provider} search response was not valid UTF-8")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> reqwest::Response {
        let mut builder = http::Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        reqwest::Response::from(builder.body(body.to_owned()).unwrap())
    }

    async fn failure(kind: WebSearchProviderKind, resp: reqwest::Response, billing: &[BillingStatus]) -> String {
        failure_from_status(resp, kind, billing).await.to_string()
    }

    #[test]
    fn settings_treat_blank_values_as_unset_and_default_the_base_url() {
        let settings = ProviderSettings::from_values(
            WebSearchProviderKind::Tavily,
            Some(" tvly-secret ".to_owned()),
            Some(" https://tavily.example/// ".to_owned()),
        );
        assert_eq!(settings.api_key.map(|key| key.0).as_deref(), Some("tvly-secret"));
        assert_eq!(settings.base_url.as_deref(), Some("https://tavily.example"));

        for kind in WebSearchProviderKind::ALL.iter().copied() {
            let settings = ProviderSettings::from_values(kind, Some("  ".to_owned()), Some(" / ".to_owned()));
            assert!(settings.api_key.is_none(), "{kind}");
            assert_eq!(settings.base_url.as_deref(), kind.default_base_url(), "{kind}");
            let settings = ProviderSettings::from_values(kind, None, None);
            assert_eq!(settings.base_url.as_deref(), kind.default_base_url(), "{kind}");
        }
    }

    #[tokio::test]
    async fn failure_names_the_key_variable_without_echoing_the_body() {
        for status in [401, 403] {
            let resp = response(status, &[], "secret upstream detail");
            let message = failure(WebSearchProviderKind::Serply, resp, &[]).await;
            assert!(
                message.starts_with("execution failed: Serply rejected the API key ("),
                "{message}"
            );
            assert!(message.ends_with("); check SERPLY_API_KEY"), "{message}");
            assert!(!message.contains("secret upstream detail"));
        }
        let message = failure(WebSearchProviderKind::Keenable, response(401, &[], ""), &[]).await;
        assert_eq!(
            message,
            "execution failed: Keenable rejected the request (401 Unauthorized); check KEENABLE_API_KEY"
        );
    }

    #[tokio::test]
    async fn rate_limit_carries_the_retry_hint() {
        let message = failure(
            WebSearchProviderKind::Serply,
            response(429, &[("retry-after", " 7 ")], ""),
            &[],
        )
        .await;
        assert_eq!(
            message,
            "execution failed: Serply rate limited the request (429 Too Many Requests); the gateway does not retry; \
             retry after 7"
        );

        let message = failure(
            WebSearchProviderKind::Tavily,
            response(429, &[("x-ratelimit-reset", "3")], ""),
            &[],
        )
        .await;
        assert!(message.ends_with("; no Retry-After header was provided"), "{message}");

        let message = failure(
            WebSearchProviderKind::Brave,
            response(429, &[("x-ratelimit-reset", "3")], ""),
            &[],
        )
        .await;
        assert_eq!(
            message,
            "execution failed: Brave Search rate limited the request (429 Too Many Requests); the gateway does not \
             retry; retry after 3"
        );
    }

    #[tokio::test]
    async fn billing_statuses_are_reported_by_code() {
        const BILLING: &[BillingStatus] = &[BillingStatus {
            code: 432,
            meaning: "the plan usage limit was exceeded",
        }];
        let message = failure(WebSearchProviderKind::Tavily, response(432, &[], "{}"), BILLING).await;
        assert_eq!(
            message,
            "execution failed: Tavily reported the plan usage limit was exceeded (432); the gateway does not retry"
        );
        let message = failure(WebSearchProviderKind::Serply, response(432, &[], "body"), &[]).await;
        assert!(
            message.starts_with("execution failed: Serply search returned 432"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn other_statuses_carry_the_upstream_body() {
        let message = failure(WebSearchProviderKind::Keenable, response(500, &[], "boom"), &[]).await;
        assert_eq!(
            message,
            "execution failed: Keenable search returned 500 Internal Server Error: boom"
        );
        let message = failure(WebSearchProviderKind::Brave, response(422, &[], "bad"), &[]).await;
        assert_eq!(
            message,
            "execution failed: Brave Search returned 422 Unprocessable Entity: bad"
        );
    }
}
