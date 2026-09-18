//! Shared search-provider contract, normalized result types, and the response
//! helpers every provider module reads its upstream replies through.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;

use futures::StreamExt;
use serde::{Deserialize, Deserializer, Serialize};

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
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Reads a provider HTTP response body, failing as soon as it exceeds
/// [`MAX_GATEWAY_TOOL_OUTPUT_BYTES`] so an oversized provider reply is never
/// buffered in full. Every provider module reads its responses through here.
pub(crate) async fn read_response_limited(
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
