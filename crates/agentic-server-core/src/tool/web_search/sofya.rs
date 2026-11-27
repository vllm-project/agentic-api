//! Sofya Search API provider for `web_search`.
//!
//! Owns request shaping against Sofya's `POST /v1/search` and the mapping of
//! its JSON envelope onto the provider-neutral [`WebSearchProviderResponse`].
//! Sofya differs from the other providers in ways the gateway adapts here
//! rather than surfacing to the model:
//!
//! - the request is a JSON body and the API key is sent only as an
//!   `Authorization: Bearer` header, never in the body;
//! - every query is one `search_depth: "snippets"`, `topic: "general"` search,
//!   the depth that returns result snippets without reading the pages, so every
//!   hit lands in the `web` section and `news` stays empty;
//! - `include_domains` / `exclude_domains` are applied server-side and are
//!   still enforced client-side through [`DomainFilter`]. Sofya takes at most
//!   [`SOFYA_MAX_DOMAINS`] bare host names per list and rejects anything else,
//!   so only such entries are forwarded and the rest is left to the filter;
//! - `max_results` is capped at [`SOFYA_MAX_RESULTS`] and clamped instead of rejected;
//! - `freshness` maps to Sofya's `freshness` (`day`, `week`, `month`, `year`,
//!   or an inclusive `YYYY-MM-DD:YYYY-MM-DD` range);
//! - `country`, `language`, `safesearch`, and the You.com-specific arguments
//!   have no Sofya parameter and are dropped.
//!
//! `Accept-Encoding` is deliberately never sent: the core `reqwest` build has no
//! `gzip` feature, so a compressed body could not be decoded.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::args::{
    Freshness, SearchArgument, SearchScope, WebSearchArguments, clean_string, log_ignored_arguments,
    retain_allowed_results,
};
use super::provider::{BillingStatus, ProviderSettings, failure_from_status};
use super::{
    ApiKey, WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, null_as_default,
    read_response_limited,
};
use crate::config::WebSearchProviderKind;
use crate::tool::domain_policy::DomainFilter;
use crate::tool::handler::ToolError;
use crate::types::tools::WebSearchToolParam;

pub(crate) const SOFYA_API_KEY: &str = WebSearchProviderKind::Sofya.default_api_key_env();

/// Largest `max_results` Sofya accepts per request.
pub(crate) const SOFYA_MAX_RESULTS: u8 = 20;

/// Most entries Sofya accepts in `include_domains` or `exclude_domains`.
pub(crate) const SOFYA_MAX_DOMAINS: usize = 10;

const SEARCH_PATH: &str = "/v1/search";
const DATE_FORMAT: &str = "%Y-%m-%d";
/// Result snippets only; `basic` also reads every page and costs more credits.
const SEARCH_DEPTH: &str = "snippets";
const TOPIC: &str = "general";
/// Response header carrying Sofya's identifier for the request.
const REQUEST_ID_HEADER: &str = "x-request-id";
/// Sofya answers `402` when the account is out of credits or the key reached
/// its spending limit.
const BILLING_STATUSES: &[BillingStatus] = &[BillingStatus {
    code: 402,
    meaning: "the account is out of credits or the key reached its spending limit",
}];

#[derive(Debug, Clone)]
pub(crate) struct SofyaSearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: String,
    max_concurrent_requests: NonZeroUsize,
}

impl SofyaSearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// counts as unset and fails at execution time; a blank base URL falls back
    /// to Sofya's public endpoint.
    pub(crate) fn from_values(
        client: Arc<reqwest::Client>,
        api_key: Option<String>,
        base_url: Option<String>,
        max_concurrent_requests: NonZeroUsize,
    ) -> Self {
        let ProviderSettings { api_key, base_url } =
            ProviderSettings::from_values(WebSearchProviderKind::Sofya, api_key, base_url);
        Self {
            client,
            api_key,
            base_url: base_url.unwrap_or_default(),
            max_concurrent_requests,
        }
    }

    fn request_builder(&self, api_key: &ApiKey, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{SEARCH_PATH}", self.base_url))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .bearer_auth(&api_key.0)
            .body(body)
    }
}

impl WebSearchProvider for SofyaSearchProvider {
    fn search<'a>(
        &'a self,
        query: &'a str,
        args: &'a WebSearchArguments,
        config: &'a WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let api_key = self
                .api_key
                .as_ref()
                .ok_or_else(|| ToolError::Config(format!("{SOFYA_API_KEY} must be set to use the web_search tool")))?;
            let request = SofyaSearchRequest::from_args_and_config(query, args, config)?;
            let body = serde_json::to_vec(&request.body)
                .map_err(|e| ToolError::Execution(format!("failed to serialize Sofya search request: {e}")))?;
            let resp = self
                .request_builder(api_key, body)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("Sofya search request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(failure_from_status(resp, WebSearchProviderKind::Sofya, BILLING_STATUSES).await);
            }

            let request_id = resp
                .headers()
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| clean_string(Some(value)));
            let response_text = read_response_limited(resp, WebSearchProviderKind::Sofya).await?;
            let response: SofyaSearchResponse = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("Sofya search returned invalid JSON: {e}")))?;
            Ok(response.into_provider_response(&request.body.query, request_id, &request.domain_filter))
        })
    }

    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        Some(self.max_concurrent_requests)
    }
}

/// Sofya's `POST /v1/search` body, derived from the model's arguments and the
/// request-level tool configuration, plus the client-side [`DomainFilter`]
/// that re-checks Sofya's own domain filtering.
#[derive(Debug, PartialEq, Eq)]
struct SofyaSearchRequest {
    body: SofyaSearchBody,
    domain_filter: DomainFilter,
}

/// JSON body sent to Sofya. Field order is the serialized order; `None` and
/// empty lists are omitted so the payload only carries what the model asked for.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct SofyaSearchBody {
    query: String,
    search_depth: &'static str,
    topic: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_results: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include_domains: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude_domains: Vec<String>,
}

impl SofyaSearchRequest {
    fn from_args_and_config(
        query: &str,
        args: &WebSearchArguments,
        config: &WebSearchToolParam,
    ) -> Result<Self, ToolError> {
        let scope = SearchScope::from_args_and_config(args, config, Some(SOFYA_MAX_RESULTS))?;
        log_ignored_arguments(WebSearchProviderKind::Sofya, args, config, IGNORED_ARGUMENTS);
        Ok(Self {
            body: SofyaSearchBody {
                query: query.trim().to_owned(),
                search_depth: SEARCH_DEPTH,
                topic: TOPIC,
                max_results: scope.count,
                freshness: args.freshness.map(sofya_freshness),
                include_domains: sofya_domains(scope.include_domains.as_deref()),
                exclude_domains: sofya_domains(scope.exclude_domains.as_deref()),
            },
            domain_filter: scope.domain_filter(),
        })
    }
}

/// The part of a domain list Sofya can apply server-side: the first
/// [`SOFYA_MAX_DOMAINS`] bare host names. Sofya answers `422` to a longer list
/// or to an entry with a scheme or path, which would fail the whole search, so
/// those entries are left to the client-side [`DomainFilter`] alone. With more
/// than [`SOFYA_MAX_DOMAINS`] allowed domains the search itself only covers the
/// first ones.
fn sofya_domains(domains: Option<&[String]>) -> Vec<String> {
    let all = domains.unwrap_or_default();
    let forwarded: Vec<String> = all
        .iter()
        .filter(|domain| is_bare_host_name(domain))
        .take(SOFYA_MAX_DOMAINS)
        .cloned()
        .collect();
    if forwarded.len() < all.len() {
        tracing::debug!(
            declared = all.len(),
            forwarded = forwarded.len(),
            "left web_search domain entries Sofya cannot take to the gateway filter"
        );
    }
    forwarded
}

/// Sofya's rule for a domain entry: dot-separated labels of ASCII letters,
/// digits, and inner hyphens ending in an alphabetic top-level domain, with an
/// optional trailing dot (`github.com`, `docs.rs`).
fn is_bare_host_name(domain: &str) -> bool {
    let host = domain.strip_suffix('.').unwrap_or(domain);
    let Some((labels, top_level)) = host.rsplit_once('.') else {
        return false;
    };
    domain.len() <= 253
        && (2..=63).contains(&top_level.len())
        && top_level.bytes().all(|byte| byte.is_ascii_alphabetic())
        && labels.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// `country`, `language`, and `safesearch` have no Sofya parameter;
/// You.com-specific arguments have no Sofya equivalent and are dropped;
/// `boost_domains` has no filtering semantics, so it is dropped too.
const IGNORED_ARGUMENTS: &[SearchArgument] = &[
    SearchArgument::Country,
    SearchArgument::Language,
    SearchArgument::Safesearch,
    SearchArgument::Livecrawl,
    SearchArgument::LivecrawlFormats,
    SearchArgument::CrawlTimeout,
    SearchArgument::BoostDomains,
];

/// Renders the typed freshness filter as Sofya's `freshness`: the same four
/// keywords, or `YYYY-MM-DD:YYYY-MM-DD`. Both sides treat the range as
/// inclusive, so the dates are sent unchanged.
fn sofya_freshness(freshness: Freshness) -> String {
    match freshness {
        Freshness::Day => "day".to_owned(),
        Freshness::Week => "week".to_owned(),
        Freshness::Month => "month".to_owned(),
        Freshness::Year => "year".to_owned(),
        Freshness::Range { from, to } => format!("{}:{}", from.format(DATE_FORMAT), to.format(DATE_FORMAT)),
    }
}

/// Sofya's `POST /v1/search` envelope. Only `results` and `elapsed_ms` are
/// modeled; `answer`, the credit counters, and unknown keys are ignored so
/// upstream additions never break the provider.
#[derive(Debug, Default, Deserialize)]
struct SofyaSearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    results: Vec<SofyaResult>,
    /// Provider-reported latency in milliseconds.
    #[serde(default)]
    elapsed_ms: Option<f64>,
}

/// One Sofya hit. `description` is the result's snippet; at the `snippets`
/// depth `content` carries the same text and is the fallback. `fetched`,
/// `content_fetched_at`, `sublinks`, and `table` are not modeled.
#[derive(Debug, Default, Deserialize)]
struct SofyaResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
}

impl From<SofyaResult> for WebSearchResult {
    fn from(result: SofyaResult) -> Self {
        Self {
            url: result.url.trim().to_owned(),
            title: clean_string(result.title.as_deref()),
            description: clean_string(result.description.as_deref())
                .or_else(|| clean_string(result.content.as_deref())),
            snippets: Vec::new(),
            page_age: clean_string(result.published_date.as_deref()),
            contents: None,
        }
    }
}

impl SofyaSearchResponse {
    fn into_provider_response(
        self,
        query: &str,
        request_id: Option<String>,
        domain_filter: &DomainFilter,
    ) -> WebSearchProviderResponse {
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
                provider: WebSearchProviderKind::Sofya,
                query: query.to_owned(),
                search_uuid: request_id,
                latency: self.elapsed_ms.map(|elapsed_ms| elapsed_ms / 1000.0),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_provider(api_key: Option<&str>, base_url: Option<&str>) -> SofyaSearchProvider {
        SofyaSearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            api_key.map(str::to_owned),
            base_url.map(str::to_owned),
            NonZeroUsize::new(5).unwrap(),
        )
    }

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    fn build_request(json: &str) -> SofyaSearchRequest {
        SofyaSearchRequest::from_args_and_config("q", &args(json), &WebSearchToolParam::default()).unwrap()
    }

    fn body_json(request: &SofyaSearchRequest) -> serde_json::Value {
        serde_json::to_value(&request.body).unwrap()
    }

    #[tokio::test]
    async fn search_without_api_key_names_the_env_var() {
        let provider = build_provider(None, None);
        let error = provider
            .search("q", &args(r#"{"query":"q"}"#), &WebSearchToolParam::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: SOFYA_API_KEY must be set to use the web_search tool"
        );
    }

    #[tokio::test]
    async fn out_of_credits_is_reported_without_retrying() {
        let resp = reqwest::Response::from(http::Response::builder().status(402).body(String::new()).unwrap());
        let error = failure_from_status(resp, WebSearchProviderKind::Sofya, BILLING_STATUSES).await;
        assert_eq!(
            error.to_string(),
            "execution failed: Sofya reported the account is out of credits or the key reached its spending limit \
             (402); the gateway does not retry"
        );
    }

    #[test]
    fn request_targets_the_search_endpoint_with_a_bearer_key() {
        let provider = build_provider(Some("k"), None);
        let request = provider
            .request_builder(provider.api_key.as_ref().unwrap(), Vec::new())
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url().as_str(), "https://sofya.co/v1/search");
        assert_eq!(request.headers()["Authorization"], "Bearer k");
        assert!(request.headers().get("Accept-Encoding").is_none());
    }

    #[test]
    fn request_renders_every_argument_in_sofya_syntax() {
        let request = SofyaSearchRequest::from_args_and_config(
            "  rust async  ",
            &args(
                r#"{"query":"rust async","count":7,"freshness":"week","language":"en-GB","safesearch":"strict","country":"gb","livecrawl":"web","exclude_domains":[" Spam.example ", ""]}"#,
            ),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(
            body_json(&request),
            serde_json::json!({
                "query": "rust async",
                "search_depth": "snippets",
                "topic": "general",
                "max_results": 7,
                "freshness": "week",
                "exclude_domains": ["Spam.example"]
            }),
            "arguments without a Sofya parameter are not forwarded"
        );
        assert!(!request.domain_filter.allows("https://spam.example/x"));
        assert!(request.domain_filter.allows("https://other.org/x"));
    }

    #[test]
    fn request_omits_optional_fields_and_clamps_count() {
        let request = build_request(r#"{"query":"q"}"#);
        assert_eq!(
            body_json(&request),
            serde_json::json!({"query": "q", "search_depth": "snippets", "topic": "general"})
        );
        assert!(request.domain_filter.is_empty());
        assert!(!serde_json::to_string(&request.body).unwrap().contains("api_key"));

        assert_eq!(
            build_request(r#"{"query":"q","count":50}"#).body.max_results,
            Some(SOFYA_MAX_RESULTS)
        );
        assert_eq!(build_request(r#"{"query":"q","count":20}"#).body.max_results, Some(20));
    }

    #[test]
    fn freshness_renders_keyword_or_inclusive_range() {
        assert_eq!(sofya_freshness(Freshness::Day), "day");
        assert_eq!(sofya_freshness(Freshness::Week), "week");
        assert_eq!(sofya_freshness(Freshness::Month), "month");
        assert_eq!(sofya_freshness(Freshness::Year), "year");
        // Both ranges are inclusive, so the dates are not widened.
        let range: Freshness = "2026-01-02to2026-02-03".parse().unwrap();
        assert_eq!(sofya_freshness(range), "2026-01-02:2026-02-03");

        let request = build_request(r#"{"query":"q","freshness":"2026-03-01to2026-03-01"}"#);
        assert_eq!(body_json(&request)["freshness"], "2026-03-01:2026-03-01");
    }

    #[test]
    fn request_forwards_only_bare_host_names_and_at_most_ten_domains() {
        for valid in ["github.com", "docs.rs", "a-b.example.co.uk", "example.com."] {
            assert!(is_bare_host_name(valid), "{valid}");
        }
        for invalid in [
            "https://github.com",
            "github.com/rust-lang",
            "localhost",
            "192.168.0.1",
            "-a.com",
            "a..com",
            "bad domain.com",
            "*.example.com",
        ] {
            assert!(!is_bare_host_name(invalid), "{invalid}");
        }

        let request =
            build_request(r#"{"query":"q","include_domains":["docs.rs","github.com/rust-lang","localhost"]}"#);
        assert_eq!(body_json(&request)["include_domains"], serde_json::json!(["docs.rs"]));
        assert!(request.domain_filter.allows("https://docs.rs/tokio"));
        assert!(!request.domain_filter.allows("https://example.com/"));

        let domains: Vec<String> = (0..12).map(|i| format!("\"d{i}.com\"")).collect();
        let request = build_request(&format!(r#"{{"query":"q","exclude_domains":[{}]}}"#, domains.join(",")));
        assert_eq!(request.body.exclude_domains.len(), SOFYA_MAX_DOMAINS);
        assert_eq!(request.body.exclude_domains[9], "d9.com");
        assert!(
            !request.domain_filter.allows("https://d11.com/x"),
            "entries past the tenth are still filtered by the gateway"
        );
    }

    #[test]
    fn response_maps_results_and_tolerates_unknown_fields() {
        let response: SofyaSearchResponse = serde_json::from_str(
            r#"{
                "query": "rust",
                "answer": null,
                "results": [
                    {
                        "title": " Rust ",
                        "url": " https://www.rust-lang.org/ ",
                        "content": "A language",
                        "description": " A language empowering everyone ",
                        "fetched": false,
                        "content_fetched_at": null,
                        "published_date": "2026-01-02",
                        "sublinks": [],
                        "table": {}
                    },
                    {"url": "https://example.com/no-snippet", "title": "", "description": "", "content": "page text"},
                    {"url": "", "title": "no url"}
                ],
                "search_depth": "snippets",
                "topic": "general",
                "elapsed_ms": 1250,
                "credits_used": 1,
                "credits_remaining": 99,
                "altered_query": null,
                "relaxed_query": null
            }"#,
        )
        .unwrap();
        let response = response.into_provider_response("rust", Some("req_123".to_owned()), &DomainFilter::default());
        assert_eq!(
            response.web,
            vec![
                WebSearchResult {
                    url: "https://www.rust-lang.org/".to_owned(),
                    title: Some("Rust".to_owned()),
                    description: Some("A language empowering everyone".to_owned()),
                    snippets: Vec::new(),
                    page_age: Some("2026-01-02".to_owned()),
                    contents: None,
                },
                WebSearchResult {
                    url: "https://example.com/no-snippet".to_owned(),
                    description: Some("page text".to_owned()),
                    ..WebSearchResult::default()
                },
            ]
        );
        assert!(response.news.is_empty());
        assert_eq!(
            serde_json::to_string(&response.metadata).unwrap(),
            r#"{"provider":"sofya","query":"rust","search_uuid":"req_123","latency":1.25}"#
        );
    }

    #[test]
    fn response_tolerates_missing_and_null_fields() {
        for body in ["{}", r#"{"results":null,"elapsed_ms":null}"#] {
            let response: SofyaSearchResponse = serde_json::from_str(body).unwrap();
            let response = response.into_provider_response("q", None, &DomainFilter::default());
            assert!(response.web.is_empty());
            assert!(response.news.is_empty());
            assert_eq!(
                serde_json::to_string(&response.metadata).unwrap(),
                r#"{"provider":"sofya","query":"q"}"#
            );
        }
    }

    #[test]
    fn response_applies_domain_filter_as_defense_in_depth() {
        let response: SofyaSearchResponse = serde_json::from_str(
            r#"{"results": [
                {"url": "https://docs.example.com/a"},
                {"url": "https://notexample.com/b"},
                {"url": "https://EXAMPLE.COM./c"},
                {"url": "not a url"}
            ]}"#,
        )
        .unwrap();
        let filter = DomainFilter::new(Some(&["example.com".to_owned()]), None);
        let response = response.into_provider_response("q", None, &filter);
        let urls: Vec<_> = response.web.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://docs.example.com/a", "https://EXAMPLE.COM./c"]);
    }
}
