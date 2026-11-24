//! Keenable Search API provider for `web_search`.
//!
//! Owns request shaping against Keenable's `POST /v1/search` (keyed) and
//! `POST /v1/search/public` (key-free) and the mapping of its JSON envelope
//! onto the provider-neutral [`WebSearchProviderResponse`]. Keenable differs
//! from the other providers in ways the gateway adapts here rather than
//! surfacing to the model:
//!
//! - an API key is optional: with `KEENABLE_API_KEY` set the request goes to
//!   `/v1/search` with an `X-API-Key` header, otherwise to the key-free
//!   `/v1/search/public` endpoint, which identifies the caller by an
//!   `X-Keenable-Title` header instead;
//! - the request is a JSON body carrying only `query` and `max_results`
//!   (capped at [`KEENABLE_MAX_RESULTS`] and clamped instead of rejected);
//! - Keenable has no server-side domain, freshness, locale, or safe-search
//!   parameters, so those arguments are dropped (domain filters are still
//!   enforced client-side through [`DomainFilter`]);
//! - every hit lands in the `web` section and `news` stays empty.
//!
//! `Accept-Encoding` is deliberately never sent: the core `reqwest` build has no
//! `gzip` feature, so a compressed body could not be decoded.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::args::{WebSearchArguments, clean_string, clean_vec, retain_allowed_results, validate_count};
use super::{
    ApiKey, WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, clean_base_url,
    null_as_default, read_response_limited,
};
use crate::config::WebSearchProviderKind;
use crate::tool::domain_policy::DomainFilter;
use crate::tool::handler::ToolError;
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};

pub(crate) const KEENABLE_API_KEY: &str = WebSearchProviderKind::Keenable.default_api_key_env();

/// Largest `max_results` Keenable accepts per request.
pub(crate) const KEENABLE_MAX_RESULTS: u8 = 20;

const SEARCH_PATH: &str = "/v1/search";
const PUBLIC_SEARCH_PATH: &str = "/v1/search/public";
/// Application name sent to the key-free endpoint.
const APP_TITLE: &str = "vLLM Agentic API";

#[derive(Debug, Clone)]
pub(crate) struct KeenableSearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: String,
    max_concurrent_requests: NonZeroUsize,
}

impl KeenableSearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// counts as unset and selects the key-free endpoint; a blank base URL
    /// falls back to Keenable's public host.
    pub(crate) fn from_values(
        client: Arc<reqwest::Client>,
        api_key: Option<String>,
        base_url: Option<String>,
        max_concurrent_requests: NonZeroUsize,
    ) -> Self {
        let api_key = api_key
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(ApiKey);
        let base_url = base_url
            .and_then(|value| clean_base_url(&value))
            .or_else(|| WebSearchProviderKind::Keenable.default_base_url().map(str::to_owned))
            .unwrap_or_default();
        Self {
            client,
            api_key,
            base_url,
            max_concurrent_requests,
        }
    }

    fn request_builder(&self, body: Vec<u8>) -> reqwest::RequestBuilder {
        let builder = match &self.api_key {
            Some(api_key) => self
                .client
                .post(format!("{}{SEARCH_PATH}", self.base_url))
                .header("X-API-Key", &api_key.0),
            None => self
                .client
                .post(format!("{}{PUBLIC_SEARCH_PATH}", self.base_url))
                .header("X-Keenable-Title", APP_TITLE),
        };
        builder
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .body(body)
    }
}

impl WebSearchProvider for KeenableSearchProvider {
    fn search<'a>(
        &'a self,
        query: &'a str,
        args: &'a WebSearchArguments,
        config: &'a WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let request = KeenableSearchRequest::from_args_and_config(query, args, config)?;
            let body = serde_json::to_vec(&request.body)
                .map_err(|e| ToolError::Execution(format!("failed to serialize Keenable search request: {e}")))?;
            let resp = self
                .request_builder(body)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("Keenable search request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(failure_from_status(resp).await);
            }

            let response_text = read_response_limited(resp, WebSearchProviderKind::Keenable).await?;
            let response: KeenableSearchResponse = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("Keenable search returned invalid JSON: {e}")))?;
            Ok(response.into_provider_response(&request.body.query, &request.domain_filter))
        })
    }

    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        Some(self.max_concurrent_requests)
    }
}

/// Maps a non-2xx Keenable response to an actionable, credential-free error.
///
/// `401`/`403` name the key variable without echoing the upstream body; `429`
/// is reported without retrying (a retry loop would blur the gateway tool
/// timeout) and carries the upstream `Retry-After` so the caller can back off.
async fn failure_from_status(resp: reqwest::Response) -> ToolError {
    let status = resp.status();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ToolError::Execution(format!(
            "Keenable rejected the request ({status}); check {KEENABLE_API_KEY}"
        )),
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let hint = retry_after.map_or_else(
                || "; no Retry-After header was provided".to_owned(),
                |value| format!("; retry after {value}"),
            );
            ToolError::Execution(format!(
                "Keenable rate limited the request ({status}); the gateway does not retry{hint}"
            ))
        }
        _ => {
            let body = read_response_limited(resp, WebSearchProviderKind::Keenable)
                .await
                .unwrap_or_default();
            ToolError::Execution(format!("Keenable search returned {status}: {body}"))
        }
    }
}

/// Keenable's request body plus the client-side [`DomainFilter`] that applies
/// the domain arguments Keenable has no server-side equivalent for.
#[derive(Debug, PartialEq, Eq)]
struct KeenableSearchRequest {
    body: KeenableSearchBody,
    domain_filter: DomainFilter,
}

/// JSON body sent to Keenable; `None` is omitted so Keenable's default applies.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct KeenableSearchBody {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_results: Option<u8>,
}

impl KeenableSearchRequest {
    fn from_args_and_config(
        query: &str,
        args: &WebSearchArguments,
        config: &WebSearchToolParam,
    ) -> Result<Self, ToolError> {
        let max_results = args
            .count
            .or_else(|| {
                config
                    .search_context_size
                    .map(WebSearchContextSize::default_count)
                    .map(u16::from)
            })
            .map(validate_count)
            .transpose()?
            .map(clamp_max_results);
        let config_domains = config
            .filters
            .as_ref()
            .and_then(|filters| clean_vec(filters.allowed_domains.as_deref()));
        let config_blocked_domains = config
            .filters
            .as_ref()
            .and_then(|filters| clean_vec(filters.blocked_domains.as_deref()));
        let include_domains = config_domains.or_else(|| args.include_domains.clone());
        let exclude_domains = config_blocked_domains.or_else(|| args.exclude_domains.clone());
        if include_domains.is_some() && (exclude_domains.is_some() || args.boost_domains.is_some()) {
            return Err(ToolError::Config(
                "include_domains cannot be combined with exclude_domains or boost_domains".to_owned(),
            ));
        }
        log_ignored_arguments(args, config);
        let domain_filter = DomainFilter::new(include_domains.as_deref(), exclude_domains.as_deref());

        Ok(Self {
            body: KeenableSearchBody {
                query: query.trim().to_owned(),
                max_results,
            },
            domain_filter,
        })
    }
}

/// Keenable accepts at most [`KEENABLE_MAX_RESULTS`] results; the model cannot
/// know provider limits, so a larger request is clamped rather than rejected.
fn clamp_max_results(count: u8) -> u8 {
    if count > KEENABLE_MAX_RESULTS {
        tracing::debug!(
            requested = count,
            max = KEENABLE_MAX_RESULTS,
            "clamped web_search count to Keenable maximum"
        );
        KEENABLE_MAX_RESULTS
    } else {
        count
    }
}

/// Arguments without a Keenable request parameter. Domain filters are not
/// listed: they are applied client-side.
fn log_ignored_arguments(args: &WebSearchArguments, config: &WebSearchToolParam) {
    let config_country = config
        .user_location
        .as_ref()
        .is_some_and(|location| clean_string(location.country.as_deref()).is_some());
    let ignored: Vec<&str> = [
        ("country", config_country || args.country.is_some()),
        ("language", args.language.is_some()),
        ("safesearch", args.safesearch.is_some()),
        ("freshness", args.freshness.is_some()),
        ("livecrawl", args.livecrawl.is_some()),
        ("livecrawl_formats", args.livecrawl_formats.is_some()),
        ("crawl_timeout", args.crawl_timeout.is_some()),
        ("boost_domains", args.boost_domains.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    if !ignored.is_empty() {
        tracing::debug!(arguments = ?ignored, "ignored web_search arguments without a Keenable equivalent");
    }
}

/// Keenable's search envelope. Only `results` is modeled; unknown keys are
/// ignored so upstream additions never break the provider.
#[derive(Debug, Default, Deserialize)]
struct KeenableSearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    results: Vec<KeenableResult>,
}

/// One Keenable hit. `snippet` is the page excerpt; `description` is the
/// page's own summary and is often empty, so it is the fallback.
#[derive(Debug, Default, Deserialize)]
struct KeenableResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
}

impl From<KeenableResult> for WebSearchResult {
    fn from(result: KeenableResult) -> Self {
        Self {
            url: result.url.trim().to_owned(),
            title: clean_string(result.title.as_deref()),
            description: clean_string(result.snippet.as_deref())
                .or_else(|| clean_string(result.description.as_deref())),
            snippets: Vec::new(),
            page_age: clean_string(result.published_at.as_deref()),
            contents: None,
        }
    }
}

impl KeenableSearchResponse {
    fn into_provider_response(self, query: &str, domain_filter: &DomainFilter) -> WebSearchProviderResponse {
        let mut web: Vec<WebSearchResult> = self
            .results
            .into_iter()
            .map(WebSearchResult::from)
            .filter(|result| !result.url.is_empty())
            .collect();
        retain_allowed_results(domain_filter, &mut web);
        WebSearchProviderResponse {
            web,
            news: Vec::new(),
            metadata: WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Keenable,
                query: query.to_owned(),
                search_uuid: None,
                latency: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::DomainFilters;

    fn build_provider(api_key: Option<&str>, base_url: Option<&str>) -> KeenableSearchProvider {
        KeenableSearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            api_key.map(str::to_owned),
            base_url.map(str::to_owned),
            NonZeroUsize::new(5).unwrap(),
        )
    }

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    #[test]
    fn provider_debug_is_redacted_and_defaults_base_url() {
        let provider = build_provider(Some("keen-super-secret"), None);
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("keen-super-secret"));
        assert_eq!(provider.base_url, "https://api.keenable.ai");
        assert_eq!(provider.max_concurrent_requests(), NonZeroUsize::new(5));

        let provider = build_provider(Some("  "), Some(" https://keenable.example/// "));
        assert!(provider.api_key.is_none());
        assert_eq!(provider.base_url, "https://keenable.example");
    }

    #[test]
    fn request_targets_public_endpoint_without_key_and_keyed_endpoint_with_key() {
        let request = build_provider(None, None).request_builder(Vec::new()).build().unwrap();
        assert_eq!(request.url().as_str(), "https://api.keenable.ai/v1/search/public");
        assert_eq!(request.headers()["X-Keenable-Title"], APP_TITLE);
        assert!(request.headers().get("X-API-Key").is_none());

        let request = build_provider(Some("k"), None)
            .request_builder(Vec::new())
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(), "https://api.keenable.ai/v1/search");
        assert_eq!(request.headers()["X-API-Key"], "k");
        assert!(request.headers().get("X-Keenable-Title").is_none());
    }

    #[test]
    fn request_body_carries_query_and_clamped_count_only() {
        let request = KeenableSearchRequest::from_args_and_config(
            "  rust async  ",
            &args(r#"{"query":"rust async","count":50,"freshness":"week","language":"en","exclude_domains":["spam.example"]}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&request.body).unwrap(),
            serde_json::json!({"query": "rust async", "max_results": 20})
        );
        assert!(!request.domain_filter.allows("https://spam.example/x"));

        let request =
            KeenableSearchRequest::from_args_and_config("q", &args(r#"{"query":"q"}"#), &WebSearchToolParam::default())
                .unwrap();
        assert_eq!(
            serde_json::to_value(&request.body).unwrap(),
            serde_json::json!({"query": "q"})
        );
    }

    #[test]
    fn config_domain_filters_apply_client_side() {
        let config = WebSearchToolParam {
            filters: Some(DomainFilters {
                allowed_domains: Some(vec!["docs.rs".to_owned()]),
                blocked_domains: None,
            }),
            ..WebSearchToolParam::default()
        };
        let request = KeenableSearchRequest::from_args_and_config("q", &args(r#"{"query":"q"}"#), &config).unwrap();
        let response: KeenableSearchResponse = serde_json::from_value(serde_json::json!({
            "results": [
                {"url": "https://docs.rs/tokio", "title": "tokio", "description": "", "snippet": "Runtime"},
                {"url": "https://other.example/x", "title": "Other"}
            ]
        }))
        .unwrap();
        let mapped = response.into_provider_response("q", &request.domain_filter);
        assert_eq!(mapped.web.len(), 1);
        assert_eq!(mapped.web[0].url, "https://docs.rs/tokio");
    }

    #[test]
    fn response_maps_snippet_with_description_fallback_and_drops_blank_urls() {
        let response: KeenableSearchResponse = serde_json::from_value(serde_json::json!({
            "results": [
                {"url": " https://a.example ", "title": " A ", "description": "", "snippet": " excerpt ", "published_at": "2026-01-02T00:00:00Z"},
                {"url": "https://b.example", "title": "B", "description": "summary", "snippet": ""},
                {"url": "", "title": "no url"}
            ]
        }))
        .unwrap();
        let mapped = response.into_provider_response("q", &DomainFilter::default());
        assert_eq!(mapped.web.len(), 2);
        assert_eq!(mapped.web[0].url, "https://a.example");
        assert_eq!(mapped.web[0].title.as_deref(), Some("A"));
        assert_eq!(mapped.web[0].description.as_deref(), Some("excerpt"));
        assert_eq!(mapped.web[0].page_age.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(mapped.web[1].description.as_deref(), Some("summary"));
        assert!(mapped.news.is_empty());
        assert_eq!(mapped.metadata.provider, WebSearchProviderKind::Keenable);

        let empty: KeenableSearchResponse = serde_json::from_str(r#"{"results":null}"#).unwrap();
        assert!(
            empty
                .into_provider_response("q", &DomainFilter::default())
                .web
                .is_empty()
        );
    }
}
