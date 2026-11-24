//! Brave Search API provider for `web_search`.
//!
//! Owns request shaping against Brave's `GET /res/v1/web/search` and the
//! mapping of its JSON envelope onto the provider-neutral
//! [`WebSearchProviderResponse`]. Brave differs from You.com in ways the
//! gateway adapts here rather than surfacing to the model:
//!
//! - no server-side domain filtering, so `include_domains` / `exclude_domains`
//!   are applied client-side through [`DomainFilter`];
//! - `count` is capped at [`BRAVE_MAX_COUNT`] and clamped instead of rejected;
//! - `freshness` uses `pd` / `pw` / `pm` / `py` short codes;
//! - the free plan allows roughly one request per second, so the provider
//!   defaults to serial queries through [`WebSearchProvider::max_concurrent_requests`].
//!
//! `Accept-Encoding` is deliberately never sent: the core `reqwest` build has no
//! `gzip` feature, so a compressed body could not be decoded.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::args::{
    Freshness, SearchArgument, SearchScope, WebSearchArguments, clean_string, clean_vec, log_ignored_arguments,
    retain_allowed_results,
};
use super::provider::{ProviderSettings, failure_from_status};
use super::{
    ApiKey, WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, null_as_default,
    read_response_limited,
};
use crate::config::WebSearchProviderKind;
use crate::tool::domain_policy::DomainFilter;
use crate::tool::handler::ToolError;
use crate::types::tools::WebSearchToolParam;

pub(crate) const BRAVE_API_KEY: &str = WebSearchProviderKind::Brave.default_api_key_env();

/// Largest `count` Brave accepts per request.
pub(crate) const BRAVE_MAX_COUNT: u8 = 20;

const SEARCH_PATH: &str = "/res/v1/web/search";
const RESULT_FILTER: &str = "web,news";

#[derive(Debug, Clone)]
pub(crate) struct BraveSearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: String,
    max_concurrent_requests: NonZeroUsize,
}

impl BraveSearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// counts as unset and fails at execution time; a blank base URL falls back
    /// to Brave's public endpoint.
    pub(crate) fn from_values(
        client: Arc<reqwest::Client>,
        api_key: Option<String>,
        base_url: Option<String>,
        max_concurrent_requests: NonZeroUsize,
    ) -> Self {
        let ProviderSettings { api_key, base_url } =
            ProviderSettings::from_values(WebSearchProviderKind::Brave, api_key, base_url);
        Self {
            client,
            api_key,
            base_url: base_url.unwrap_or_default(),
            max_concurrent_requests,
        }
    }
}

impl WebSearchProvider for BraveSearchProvider {
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
                .ok_or_else(|| ToolError::Config(format!("{BRAVE_API_KEY} must be set to use the web_search tool")))?;
            let request = BraveSearchRequest::from_args_and_config(query, args, config)?;
            let resp = self
                .client
                .get(format!("{}{SEARCH_PATH}", self.base_url))
                .query(&request.query_params())
                .header("Accept", "application/json")
                .header("X-Subscription-Token", &api_key.0)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("Brave Search request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(failure_from_status(resp, WebSearchProviderKind::Brave, &[]).await);
            }

            let response_text = read_response_limited(resp, WebSearchProviderKind::Brave).await?;
            let response: BraveSearchResponse = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("Brave Search returned invalid JSON: {e}")))?;
            Ok(response.into_provider_response(&request.query, &request.domain_filter))
        })
    }

    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        Some(self.max_concurrent_requests)
    }
}

/// Query parameters for Brave's `GET /res/v1/web/search`, derived from the
/// model's arguments and the request-level tool configuration, plus the
/// client-side [`DomainFilter`] Brave cannot apply itself.
#[derive(Debug, PartialEq, Eq)]
struct BraveSearchRequest {
    query: String,
    count: Option<u8>,
    freshness: Option<Freshness>,
    country: Option<String>,
    search_lang: Option<String>,
    safesearch: Option<String>,
    domain_filter: DomainFilter,
}

impl BraveSearchRequest {
    fn query_params(&self) -> Vec<(String, String)> {
        let mut params = vec![
            ("q".to_owned(), self.query.clone()),
            ("result_filter".to_owned(), RESULT_FILTER.to_owned()),
            // Brave wraps matched terms in `<strong>` unless asked not to; the
            // model should see plain text.
            ("text_decorations".to_owned(), "false".to_owned()),
        ];
        if let Some(count) = self.count {
            params.push(("count".to_owned(), count.to_string()));
        }
        if let Some(freshness) = &self.freshness {
            params.push(("freshness".to_owned(), brave_freshness(*freshness)));
        }
        if let Some(country) = &self.country {
            params.push(("country".to_owned(), country.clone()));
        }
        if let Some(search_lang) = &self.search_lang {
            params.push(("search_lang".to_owned(), search_lang.clone()));
        }
        if let Some(safesearch) = &self.safesearch {
            params.push(("safesearch".to_owned(), safesearch.clone()));
        }
        params
    }

    fn from_args_and_config(
        query: &str,
        args: &WebSearchArguments,
        config: &WebSearchToolParam,
    ) -> Result<Self, ToolError> {
        let scope = SearchScope::from_args_and_config(args, config, Some(BRAVE_MAX_COUNT))?;
        log_ignored_arguments(WebSearchProviderKind::Brave, args, config, IGNORED_ARGUMENTS);
        let country = config
            .user_location
            .as_ref()
            .and_then(|location| clean_string(location.country.as_deref()))
            .or_else(|| args.country.clone())
            .map(|value| value.to_ascii_uppercase());

        Ok(Self {
            query: query.trim().to_owned(),
            count: scope.count,
            freshness: args.freshness,
            country,
            // Brave expects lowercase codes such as `en`, `pt-br`, `zh-hans`.
            search_lang: args.language.as_deref().map(str::to_ascii_lowercase),
            safesearch: args.safesearch.clone(),
            domain_filter: scope.domain_filter(),
        })
    }
}

/// You.com-specific arguments have no Brave equivalent and are dropped;
/// `boost_domains` has no filtering semantics, so it is dropped too.
const IGNORED_ARGUMENTS: &[SearchArgument] = &[
    SearchArgument::Livecrawl,
    SearchArgument::LivecrawlFormats,
    SearchArgument::CrawlTimeout,
    SearchArgument::BoostDomains,
];

/// Renders the typed freshness filter in Brave's syntax.
fn brave_freshness(freshness: Freshness) -> String {
    match freshness {
        Freshness::Day => "pd".to_owned(),
        Freshness::Week => "pw".to_owned(),
        Freshness::Month => "pm".to_owned(),
        Freshness::Year => "py".to_owned(),
        range @ Freshness::Range { .. } => range.to_string(),
    }
}

/// Brave's `GET /res/v1/web/search` envelope. Only the `web` and `news`
/// sections are modeled; other sections (`mixed`, `query`, `videos`, …) and
/// unknown keys are ignored so upstream additions never break the provider.
#[derive(Debug, Default, Deserialize)]
struct BraveSearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    web: BraveResultSection,
    #[serde(default, deserialize_with = "null_as_default")]
    news: BraveResultSection,
}

#[derive(Debug, Default, Deserialize)]
struct BraveResultSection {
    #[serde(default, deserialize_with = "null_as_default")]
    results: Vec<BraveResult>,
}

/// One Brave web or news hit. `page_age` is Brave's ISO timestamp and `age` its
/// human-readable form; cosmetic fields (`thumbnail`, `meta_url`, `profile`)
/// are not modeled.
#[derive(Debug, Default, Deserialize)]
struct BraveResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    page_age: Option<String>,
    #[serde(default)]
    age: Option<String>,
    /// Additional excerpts, returned only on plans that enable them.
    #[serde(default, deserialize_with = "null_as_default")]
    extra_snippets: Vec<String>,
}

impl From<BraveResult> for WebSearchResult {
    fn from(result: BraveResult) -> Self {
        Self {
            url: result.url.trim().to_owned(),
            title: clean_string(result.title.as_deref()),
            description: clean_string(result.description.as_deref()),
            snippets: clean_vec(Some(&result.extra_snippets)).unwrap_or_default(),
            page_age: clean_string(result.page_age.as_deref()).or_else(|| clean_string(result.age.as_deref())),
            contents: None,
        }
    }
}

impl BraveSearchResponse {
    fn into_provider_response(self, query: &str, domain_filter: &DomainFilter) -> WebSearchProviderResponse {
        let mut web: Vec<WebSearchResult> = self.web.results.into_iter().map(Into::into).collect();
        let mut news: Vec<WebSearchResult> = self.news.results.into_iter().map(Into::into).collect();
        retain_allowed_results(domain_filter, &mut web);
        retain_allowed_results(domain_filter, &mut news);
        WebSearchProviderResponse {
            web,
            news,
            metadata: WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Brave,
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
    use crate::types::tools::{DomainFilters, WebSearchContextSize, WebSearchUserLocation};

    fn build_provider(api_key: Option<&str>, base_url: Option<&str>) -> BraveSearchProvider {
        BraveSearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            api_key.map(str::to_owned),
            base_url.map(str::to_owned),
            NonZeroUsize::new(1).unwrap(),
        )
    }

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    fn params(request: &BraveSearchRequest) -> Vec<(String, String)> {
        request.query_params()
    }

    #[test]
    fn provider_debug_is_redacted_and_defaults_base_url() {
        let provider = build_provider(Some("super-secret-key"), None);
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("super-secret-key"));
        assert!(rendered.contains("ApiKey(<redacted>)"));
        assert_eq!(provider.base_url, "https://api.search.brave.com");
        assert_eq!(provider.max_concurrent_requests(), NonZeroUsize::new(1));

        let provider = build_provider(Some("  "), Some(" https://brave.example/// "));
        assert!(provider.api_key.is_none());
        assert_eq!(provider.base_url, "https://brave.example");
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
            "invalid tool config: BRAVE_API_KEY must be set to use the web_search tool"
        );
    }

    #[test]
    fn request_renders_every_argument_in_brave_syntax() {
        let request = BraveSearchRequest::from_args_and_config(
            "  rust async  ",
            &args(
                r#"{"query":"rust async","count":7,"freshness":"week","country":"gb","language":"en-GB","safesearch":"strict"}"#,
            ),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(
            params(&request),
            [
                ("q", "rust async"),
                ("result_filter", "web,news"),
                ("text_decorations", "false"),
                ("count", "7"),
                ("freshness", "pw"),
                ("country", "GB"),
                ("search_lang", "en-gb"),
                ("safesearch", "strict"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        assert!(request.domain_filter.is_empty());
    }

    #[test]
    fn request_clamps_count_and_applies_context_size_default() {
        let request = BraveSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","count":50}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(request.count, Some(BRAVE_MAX_COUNT));

        let request = BraveSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","count":20}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(request.count, Some(20));

        let config = WebSearchToolParam {
            search_context_size: Some(WebSearchContextSize::High),
            ..WebSearchToolParam::default()
        };
        let request = BraveSearchRequest::from_args_and_config("q", &args(r#"{"query":"q"}"#), &config).unwrap();
        assert_eq!(
            request.count.map(u16::from),
            Some(u16::from(WebSearchContextSize::High.default_count()))
        );

        let error = BraveSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","count":0}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_search count must be between 1 and 100"
        );
    }

    #[test]
    fn freshness_renders_short_codes_and_ranges() {
        assert_eq!(brave_freshness(Freshness::Day), "pd");
        assert_eq!(brave_freshness(Freshness::Week), "pw");
        assert_eq!(brave_freshness(Freshness::Month), "pm");
        assert_eq!(brave_freshness(Freshness::Year), "py");
        let range: Freshness = "2026-01-02to2026-02-03".parse().unwrap();
        assert_eq!(brave_freshness(range), "2026-01-02to2026-02-03");
    }

    #[test]
    fn request_ignores_you_specific_arguments_and_builds_domain_filter() {
        let request = BraveSearchRequest::from_args_and_config(
            "q",
            &args(
                r#"{"query":"q","livecrawl":"web","livecrawl_formats":["markdown"],"crawl_timeout":5,"exclude_domains":["Example.com"]}"#,
            ),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        let rendered = params(&request);
        assert!(
            rendered
                .iter()
                .all(|(key, _)| !key.starts_with("livecrawl") && key != "crawl_timeout")
        );
        assert!(rendered.iter().all(|(key, _)| !key.contains("domains")));
        assert!(!request.domain_filter.allows("https://docs.example.com/x"));
        assert!(request.domain_filter.allows("https://other.org/x"));
    }

    #[test]
    fn request_prefers_tool_config_filters_and_location_over_arguments() {
        let config = WebSearchToolParam {
            filters: Some(DomainFilters {
                allowed_domains: Some(vec!["rust-lang.org".to_owned()]),
                blocked_domains: None,
            }),
            user_location: Some(WebSearchUserLocation {
                country: Some("us".to_owned()),
                ..WebSearchUserLocation::default()
            }),
            ..WebSearchToolParam::default()
        };
        let request = BraveSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","country":"de","include_domains":["example.com"]}"#),
            &config,
        )
        .unwrap();
        assert_eq!(request.country.as_deref(), Some("US"));
        assert!(request.domain_filter.allows("https://doc.rust-lang.org/book"));
        assert!(!request.domain_filter.allows("https://example.com/"));
    }

    #[test]
    fn request_rejects_conflicting_domain_lists() {
        let error = BraveSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","include_domains":["a.com"],"exclude_domains":["b.com"]}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains"
        );
        let error = BraveSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","include_domains":["a.com"],"boost_domains":["b.com"]}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn response_maps_web_and_news_and_tolerates_unknown_fields() {
        let response: BraveSearchResponse = serde_json::from_str(
            r#"{
                "type": "search",
                "query": {"original": "rust"},
                "mixed": {"type": "mixed", "main": []},
                "web": {
                    "type": "search",
                    "results": [
                        {
                            "title": " Rust ",
                            "url": " https://www.rust-lang.org/ ",
                            "description": "A language",
                            "page_age": "2026-01-02T03:04:05",
                            "age": "2 days ago",
                            "language": "en",
                            "family_friendly": true,
                            "extra_snippets": ["one", " ", "two"],
                            "thumbnail": {"src": "x"},
                            "meta_url": {"hostname": "rust-lang.org"}
                        },
                        {"url": "https://example.com/no-title", "title": "", "description": null}
                    ]
                },
                "news": {
                    "type": "news",
                    "results": [
                        {"title": "Release", "url": "https://blog.rust-lang.org/1", "age": "1 hour ago",
                         "source": "Rust Blog", "breaking": false}
                    ]
                }
            }"#,
        )
        .unwrap();
        let response = response.into_provider_response("rust", &DomainFilter::default());
        assert_eq!(
            response.web,
            vec![
                WebSearchResult {
                    url: "https://www.rust-lang.org/".to_owned(),
                    title: Some("Rust".to_owned()),
                    description: Some("A language".to_owned()),
                    snippets: vec!["one".to_owned(), "two".to_owned()],
                    page_age: Some("2026-01-02T03:04:05".to_owned()),
                    contents: None,
                },
                WebSearchResult {
                    url: "https://example.com/no-title".to_owned(),
                    ..WebSearchResult::default()
                },
            ]
        );
        assert_eq!(
            response.news,
            vec![WebSearchResult {
                url: "https://blog.rust-lang.org/1".to_owned(),
                title: Some("Release".to_owned()),
                page_age: Some("1 hour ago".to_owned()),
                ..WebSearchResult::default()
            }]
        );
        assert_eq!(
            response.metadata,
            WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Brave,
                query: "rust".to_owned(),
                search_uuid: None,
                latency: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&response.metadata).unwrap(),
            r#"{"provider":"brave","query":"rust"}"#
        );
    }

    #[test]
    fn response_tolerates_missing_and_null_sections() {
        for body in ["{}", r#"{"web":null,"news":null}"#, r#"{"web":{"results":null}}"#] {
            let response: BraveSearchResponse = serde_json::from_str(body).unwrap();
            let response = response.into_provider_response("q", &DomainFilter::default());
            assert!(response.web.is_empty());
            assert!(response.news.is_empty());
        }
    }

    #[test]
    fn response_applies_domain_filter_to_both_sections() {
        let response: BraveSearchResponse = serde_json::from_str(
            r#"{
                "web": {"results": [
                    {"url": "https://docs.example.com/a"},
                    {"url": "https://notexample.com/b"},
                    {"url": "https://EXAMPLE.COM./c"},
                    {"url": "not a url"}
                ]},
                "news": {"results": [
                    {"url": "https://news.example.com/d"},
                    {"url": "https://other.org/e"}
                ]}
            }"#,
        )
        .unwrap();
        let filter = DomainFilter::new(Some(&["example.com".to_owned()]), None);
        let response = response.into_provider_response("q", &filter);
        let urls = |results: &[WebSearchResult]| results.iter().map(|r| r.url.clone()).collect::<Vec<_>>();
        assert_eq!(
            urls(&response.web),
            ["https://docs.example.com/a", "https://EXAMPLE.COM./c"]
        );
        assert_eq!(urls(&response.news), ["https://news.example.com/d"]);
    }
}
