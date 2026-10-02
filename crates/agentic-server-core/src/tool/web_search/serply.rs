//! Serply Search API provider for `web_search`.
//!
//! Owns request shaping against Serply's `GET /v1/search` and the mapping of
//! its JSON envelope onto the provider-neutral [`WebSearchProviderResponse`].
//! Serply returns Google results, so it differs from the other providers in
//! ways the gateway adapts here rather than surfacing to the model:
//!
//! - the API key is sent only as an `X-Api-Key` header, never in the URL;
//! - domain lists become `site:` / `-site:` operators in the query so Google
//!   spends its result budget on matching hosts, and are still enforced
//!   client-side through [`DomainFilter`] as defense in depth;
//! - `count` is capped at [`SERPLY_MAX_COUNT`] and clamped instead of rejected;
//!   Serply treats `num` as a hint, so the mapped results are also truncated;
//! - `freshness` maps to Google's `tbs=qdr:*`; an explicit range becomes
//!   `after:` / `before:` operators because a custom `tbs` range is not passed through;
//! - recency-filtered responses can carry `https://www.google.com/url?q=...`
//!   redirect links, which are unwrapped to the target URL;
//! - every hit lands in the `web` section and `news` stays empty.
//!
//! `Accept-Encoding` is deliberately never sent: the core `reqwest` build has no
//! `gzip` feature, so a compressed body could not be decoded.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use reqwest::StatusCode;
use serde::Deserialize;

use super::args::{DomainFilter, Freshness, WebSearchArguments, clean_string, clean_vec, validate_count};
use super::{
    ApiKey, WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, clean_base_url,
    null_as_default, read_response_limited,
};
use crate::config::WebSearchProviderKind;
use crate::tool::handler::ToolError;
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};

pub(crate) const SERPLY_API_KEY: &str = WebSearchProviderKind::Serply.default_api_key_env();

/// Largest `num` Serply honors per request.
pub(crate) const SERPLY_MAX_COUNT: u8 = 10;

const SEARCH_PATH: &str = "/v1/search";
const DATE_FORMAT: &str = "%Y-%m-%d";

#[derive(Debug, Clone)]
pub(crate) struct SerplySearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: String,
    max_concurrent_requests: NonZeroUsize,
}

impl SerplySearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// counts as unset and fails at execution time; a blank base URL falls back
    /// to Serply's public endpoint.
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
            .or_else(|| WebSearchProviderKind::Serply.default_base_url().map(str::to_owned))
            .unwrap_or_default();
        Self {
            client,
            api_key,
            base_url,
            max_concurrent_requests,
        }
    }
}

impl WebSearchProvider for SerplySearchProvider {
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
                .ok_or_else(|| ToolError::Config(format!("{SERPLY_API_KEY} must be set to use the web_search tool")))?;
            let request = SerplySearchRequest::from_args_and_config(query, args, config)?;
            let resp = self
                .client
                .get(format!("{}{SEARCH_PATH}", self.base_url))
                .query(&request.query_params())
                .header("Accept", "application/json")
                .header("X-Api-Key", &api_key.0)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("Serply search request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(failure_from_status(resp).await);
            }

            let response_text = read_response_limited(resp, WebSearchProviderKind::Serply).await?;
            let response: SerplySearchResponse = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("Serply search returned invalid JSON: {e}")))?;
            Ok(response.into_provider_response(query.trim(), &request))
        })
    }

    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        Some(self.max_concurrent_requests)
    }
}

/// Maps a non-2xx Serply response to an actionable, credential-free error.
///
/// `401`/`403` name the key variable without echoing the upstream body; `429`
/// is reported without retrying (a retry loop would blur the gateway tool
/// timeout) and carries the upstream `Retry-After` so the caller can back off.
async fn failure_from_status(resp: reqwest::Response) -> ToolError {
    let status = resp.status();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ToolError::Execution(format!(
            "Serply rejected the API key ({status}); check {SERPLY_API_KEY}"
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
                "Serply rate limited the request ({status}); the gateway does not retry{hint}"
            ))
        }
        _ => {
            let body = read_response_limited(resp, WebSearchProviderKind::Serply)
                .await
                .unwrap_or_default();
            ToolError::Execution(format!("Serply search returned {status}: {body}"))
        }
    }
}

/// Query parameters for Serply's `GET /v1/search`, derived from the model's
/// arguments and the request-level tool configuration, plus the client-side
/// [`DomainFilter`] that re-checks the `site:` operators.
#[derive(Debug, PartialEq, Eq)]
struct SerplySearchRequest {
    /// The query as sent, including any `site:` and date operators.
    q: String,
    num: Option<u8>,
    tbs: Option<&'static str>,
    gl: Option<String>,
    hl: Option<String>,
    safe: Option<&'static str>,
    domain_filter: DomainFilter,
}

impl SerplySearchRequest {
    fn query_params(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![("q", self.q.clone())];
        if let Some(num) = self.num {
            params.push(("num", num.to_string()));
        }
        if let Some(tbs) = self.tbs {
            params.push(("tbs", tbs.to_owned()));
        }
        if let Some(gl) = &self.gl {
            params.push(("gl", gl.clone()));
        }
        if let Some(hl) = &self.hl {
            params.push(("hl", hl.clone()));
        }
        if let Some(safe) = self.safe {
            params.push(("safe", safe.to_owned()));
        }
        params
    }

    fn from_args_and_config(
        query: &str,
        args: &WebSearchArguments,
        config: &WebSearchToolParam,
    ) -> Result<Self, ToolError> {
        let num = args
            .count
            .or_else(|| {
                config
                    .search_context_size
                    .map(WebSearchContextSize::default_count)
                    .map(u16::from)
            })
            .map(validate_count)
            .transpose()?
            .map(clamp_count);
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
        log_ignored_arguments(args);
        let gl = config
            .user_location
            .as_ref()
            .and_then(|location| clean_string(location.country.as_deref()))
            .or_else(|| args.country.clone())
            .map(|value| value.to_ascii_lowercase());

        let mut q = query.trim().to_owned();
        push_site_operators(&mut q, include_domains.as_deref(), exclude_domains.as_deref());
        let tbs = match args.freshness {
            Some(Freshness::Range { from, to }) => {
                // Google's `before:` is exclusive, so the inclusive range end is widened by a day.
                let before = to.succ_opt().unwrap_or(to);
                q.push_str(" after:");
                q.push_str(&from.format(DATE_FORMAT).to_string());
                q.push_str(" before:");
                q.push_str(&before.format(DATE_FORMAT).to_string());
                None
            }
            Some(freshness) => serply_tbs(freshness),
            None => None,
        };

        Ok(Self {
            q,
            num,
            tbs,
            gl,
            hl: args.language.clone(),
            safe: args
                .safesearch
                .as_deref()
                .filter(|value| value.eq_ignore_ascii_case("strict"))
                .map(|_| "active"),
            domain_filter: DomainFilter::new(include_domains.as_deref(), exclude_domains.as_deref()),
        })
    }
}

/// Appends `site:a OR site:b` for an allowlist or `-site:a -site:b` for a
/// blocklist. A domain containing whitespace could inject other operators, so
/// it is left to the client-side [`DomainFilter`] alone.
fn push_site_operators(q: &mut String, include: Option<&[String]>, exclude: Option<&[String]>) {
    let operand = |domain: &&String| !domain.contains(char::is_whitespace);
    if let Some(include) = include {
        let sites: Vec<String> = include
            .iter()
            .filter(operand)
            .map(|domain| format!("site:{domain}"))
            .collect();
        if !sites.is_empty() {
            q.push(' ');
            q.push_str(&sites.join(" OR "));
        }
    }
    for domain in exclude.unwrap_or_default().iter().filter(operand) {
        q.push_str(" -site:");
        q.push_str(domain);
    }
}

/// Serply honors at most [`SERPLY_MAX_COUNT`] results; the model cannot know
/// provider limits, so a larger request is clamped rather than rejected.
fn clamp_count(count: u8) -> u8 {
    if count > SERPLY_MAX_COUNT {
        tracing::debug!(
            requested = count,
            max = SERPLY_MAX_COUNT,
            "clamped web_search count to Serply maximum"
        );
        SERPLY_MAX_COUNT
    } else {
        count
    }
}

/// You.com-specific arguments have no Serply equivalent and are dropped;
/// `boost_domains` has no filtering semantics, so it is dropped too.
fn log_ignored_arguments(args: &WebSearchArguments) {
    let ignored: Vec<&str> = [
        ("livecrawl", args.livecrawl.is_some()),
        ("livecrawl_formats", args.livecrawl_formats.is_some()),
        ("crawl_timeout", args.crawl_timeout.is_some()),
        ("boost_domains", args.boost_domains.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    if !ignored.is_empty() {
        tracing::debug!(arguments = ?ignored, "ignored web_search arguments without a Serply equivalent");
    }
}

/// Renders a relative freshness filter as Google's `tbs` value; a range is
/// expressed as query operators instead and has no `tbs`.
fn serply_tbs(freshness: Freshness) -> Option<&'static str> {
    match freshness {
        Freshness::Day => Some("qdr:d"),
        Freshness::Week => Some("qdr:w"),
        Freshness::Month => Some("qdr:m"),
        Freshness::Year => Some("qdr:y"),
        Freshness::Range { .. } => None,
    }
}

/// Returns the target of a Google `/url?q=...` redirect link, or the link
/// unchanged when it is not one.
fn unwrap_google_redirect(link: &str) -> String {
    let link = link.trim();
    let Ok(url) = url::Url::parse(link) else {
        return link.to_owned();
    };
    let is_google = url
        .host_str()
        .is_some_and(|host| host == "google.com" || host == "www.google.com");
    if !is_google || url.path() != "/url" {
        return link.to_owned();
    }
    url.query_pairs()
        .find(|(key, value)| matches!(key.as_ref(), "q" | "url") && value.starts_with("http"))
        .map_or_else(|| link.to_owned(), |(_, value)| value.into_owned())
}

/// Serply's `GET /v1/search` envelope. Only `results` and the `ts` latency are
/// modeled; ads, knowledge graph, related searches, and unknown keys are
/// ignored so upstream additions never break the provider.
#[derive(Debug, Default, Deserialize)]
struct SerplySearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    results: Vec<SerplyResult>,
    /// Provider-reported latency in seconds.
    #[serde(default)]
    ts: Option<f64>,
}

/// One Serply hit. `position`, `result_type`, and display metadata are not modeled.
#[derive(Debug, Default, Deserialize)]
struct SerplyResult {
    #[serde(default)]
    link: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    metadata: SerplyResultMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SerplyResultMetadata {
    /// Human-readable age such as `2 days ago`, present on some results.
    #[serde(default)]
    published_time: Option<String>,
}

impl From<SerplyResult> for WebSearchResult {
    fn from(result: SerplyResult) -> Self {
        Self {
            url: unwrap_google_redirect(&result.link),
            title: clean_string(result.title.as_deref()),
            description: clean_string(result.description.as_deref()),
            snippets: Vec::new(),
            page_age: clean_string(result.metadata.published_time.as_deref()),
            contents: None,
        }
    }
}

impl SerplySearchResponse {
    fn into_provider_response(self, query: &str, request: &SerplySearchRequest) -> WebSearchProviderResponse {
        let mut web: Vec<WebSearchResult> = self.results.into_iter().map(Into::into).collect();
        request.domain_filter.retain(&mut web);
        if let Some(num) = request.num {
            web.truncate(usize::from(num));
        }
        WebSearchProviderResponse {
            web,
            news: Vec::new(),
            metadata: WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Serply,
                query: query.to_owned(),
                search_uuid: None,
                latency: self.ts,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::{WebSearchFilters, WebSearchUserLocation};

    fn build_provider(api_key: Option<&str>, base_url: Option<&str>) -> SerplySearchProvider {
        SerplySearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            api_key.map(str::to_owned),
            base_url.map(str::to_owned),
            NonZeroUsize::new(5).unwrap(),
        )
    }

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    fn build_request(json: &str) -> SerplySearchRequest {
        SerplySearchRequest::from_args_and_config("q", &args(json), &WebSearchToolParam::default()).unwrap()
    }

    #[test]
    fn provider_debug_is_redacted_and_defaults_base_url() {
        let provider = build_provider(Some("serply-super-secret"), None);
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("serply-super-secret"));
        assert!(rendered.contains("ApiKey(<redacted>)"));
        assert_eq!(provider.base_url, "https://api.serply.io");
        assert_eq!(provider.max_concurrent_requests(), NonZeroUsize::new(5));

        let provider = build_provider(Some("  "), Some(" https://serply.example/// "));
        assert!(provider.api_key.is_none());
        assert_eq!(provider.base_url, "https://serply.example");
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
            "invalid tool config: SERPLY_API_KEY must be set to use the web_search tool"
        );
    }

    #[test]
    fn request_renders_every_argument_in_serply_syntax() {
        let request = SerplySearchRequest::from_args_and_config(
            "  rust async  ",
            &args(
                r#"{"query":"rust async","count":7,"freshness":"week","country":"GB","language":"en","safesearch":"strict","livecrawl":"web"}"#,
            ),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(
            request.query_params(),
            [
                ("q", "rust async"),
                ("num", "7"),
                ("tbs", "qdr:w"),
                ("gl", "gb"),
                ("hl", "en"),
                ("safe", "active"),
            ]
            .map(|(key, value)| (key, value.to_owned()))
        );
        assert!(request.domain_filter.is_empty());
        assert_eq!(
            build_request("{\"query\":\"q\"}").query_params(),
            [("q", "q".to_owned())]
        );
        assert_eq!(build_request(r#"{"query":"q","safesearch":"moderate"}"#).safe, None);
    }

    #[test]
    fn request_clamps_count_and_applies_context_size_default() {
        assert_eq!(build_request(r#"{"query":"q","count":50}"#).num, Some(SERPLY_MAX_COUNT));
        assert_eq!(build_request(r#"{"query":"q","count":10}"#).num, Some(10));

        let config = WebSearchToolParam {
            search_context_size: Some(WebSearchContextSize::Low),
            ..WebSearchToolParam::default()
        };
        let request = SerplySearchRequest::from_args_and_config("q", &args(r#"{"query":"q"}"#), &config).unwrap();
        assert_eq!(
            request.num.map(u16::from),
            Some(u16::from(WebSearchContextSize::Low.default_count()))
        );

        let error = SerplySearchRequest::from_args_and_config(
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
    fn freshness_renders_tbs_or_date_operators() {
        assert_eq!(serply_tbs(Freshness::Day), Some("qdr:d"));
        assert_eq!(serply_tbs(Freshness::Week), Some("qdr:w"));
        assert_eq!(serply_tbs(Freshness::Month), Some("qdr:m"));
        assert_eq!(serply_tbs(Freshness::Year), Some("qdr:y"));

        let request = build_request(r#"{"query":"q","freshness":"2026-01-02to2026-12-31"}"#);
        assert_eq!(request.q, "q after:2026-01-02 before:2027-01-01");
        assert_eq!(request.tbs, None);
    }

    #[test]
    fn request_renders_domain_lists_as_site_operators() {
        let request = build_request(r#"{"query":"q","include_domains":["docs.rs"," github.com ","bad domain"]}"#);
        assert_eq!(request.q, "q site:docs.rs OR site:github.com");
        assert!(!request.domain_filter.allows("https://example.com/"));

        let request = build_request(r#"{"query":"q","exclude_domains":["a.com","b.com"]}"#);
        assert_eq!(request.q, "q -site:a.com -site:b.com");
        assert!(!request.domain_filter.allows("https://docs.a.com/x"));
        assert!(request.domain_filter.allows("https://c.com/x"));
    }

    #[test]
    fn request_prefers_tool_config_filters_and_location_over_arguments() {
        let config = WebSearchToolParam {
            filters: Some(WebSearchFilters {
                allowed_domains: Some(vec!["rust-lang.org".to_owned()]),
                blocked_domains: None,
            }),
            user_location: Some(WebSearchUserLocation {
                country: Some("US".to_owned()),
                ..WebSearchUserLocation::default()
            }),
            ..WebSearchToolParam::default()
        };
        let request = SerplySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","country":"de","include_domains":["example.com"]}"#),
            &config,
        )
        .unwrap();
        assert_eq!(request.gl.as_deref(), Some("us"));
        assert_eq!(request.q, "q site:rust-lang.org");
        assert!(request.domain_filter.allows("https://doc.rust-lang.org/book"));
        assert!(!request.domain_filter.allows("https://example.com/"));
    }

    #[test]
    fn request_rejects_conflicting_domain_lists() {
        for json in [
            r#"{"query":"q","include_domains":["a.com"],"exclude_domains":["b.com"]}"#,
            r#"{"query":"q","include_domains":["a.com"],"boost_domains":["b.com"]}"#,
        ] {
            let error = SerplySearchRequest::from_args_and_config("q", &args(json), &WebSearchToolParam::default())
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains"
            );
        }
    }

    #[test]
    fn google_redirect_links_are_unwrapped() {
        assert_eq!(
            unwrap_google_redirect("https://www.google.com/url?opi=1&q=https://blog.rust-lang.org/x%3Fa%3D1&sa=U"),
            "https://blog.rust-lang.org/x?a=1"
        );
        assert_eq!(
            unwrap_google_redirect(" https://www.google.com/search?q=https://a.com "),
            "https://www.google.com/search?q=https://a.com"
        );
        assert_eq!(
            unwrap_google_redirect("https://a.com/url?q=https://b.com"),
            "https://a.com/url?q=https://b.com"
        );
        assert_eq!(unwrap_google_redirect("not a url"), "not a url");
    }

    #[test]
    fn response_maps_results_and_tolerates_unknown_fields() {
        let response: SerplySearchResponse = serde_json::from_str(
            r#"{
                "results": [
                    {
                        "title": " Rust ",
                        "link": " https://www.rust-lang.org/ ",
                        "description": "A language",
                        "position": 1,
                        "realPosition": 1,
                        "result_type": "organic",
                        "metadata": {"display_url": "rust-lang.org", "published_time": "2 days ago"}
                    },
                    {"link": "https://www.google.com/url?q=https://example.com/no-title&sa=U", "title": ""},
                    {"link": "https://docs.rs/tokio", "metadata": null}
                ],
                "ads": [],
                "knowledge_graph": {},
                "related_searches": {"text": []},
                "ts": 1.25,
                "query": "q=rust"
            }"#,
        )
        .unwrap();
        let response = response.into_provider_response("rust", &build_request(r#"{"query":"q","count":2}"#));
        assert_eq!(
            response.web,
            vec![
                WebSearchResult {
                    url: "https://www.rust-lang.org/".to_owned(),
                    title: Some("Rust".to_owned()),
                    description: Some("A language".to_owned()),
                    snippets: Vec::new(),
                    page_age: Some("2 days ago".to_owned()),
                    contents: None,
                },
                WebSearchResult {
                    url: "https://example.com/no-title".to_owned(),
                    ..WebSearchResult::default()
                },
            ],
            "results beyond the requested count are truncated"
        );
        assert!(response.news.is_empty());
        assert_eq!(
            serde_json::to_string(&response.metadata).unwrap(),
            r#"{"provider":"serply","query":"rust","latency":1.25}"#
        );
    }

    #[test]
    fn response_tolerates_missing_and_null_fields() {
        for body in ["{}", r#"{"results":null,"ts":null}"#] {
            let response: SerplySearchResponse = serde_json::from_str(body).unwrap();
            let response = response.into_provider_response("q", &build_request(r#"{"query":"q"}"#));
            assert!(response.web.is_empty());
            assert_eq!(
                serde_json::to_string(&response.metadata).unwrap(),
                r#"{"provider":"serply","query":"q"}"#
            );
        }
    }

    #[test]
    fn response_applies_domain_filter_after_unwrapping_redirects() {
        let response: SerplySearchResponse = serde_json::from_str(
            r#"{"results": [
                {"link": "https://docs.example.com/a"},
                {"link": "https://www.google.com/url?q=https://notexample.com/b"},
                {"link": "https://www.google.com/url?q=https://example.com/c"},
                {"link": "not a url"}
            ]}"#,
        )
        .unwrap();
        let response = response.into_provider_response(
            "q",
            &build_request(r#"{"query":"q","include_domains":["example.com"]}"#),
        );
        let urls: Vec<_> = response.web.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://docs.example.com/a", "https://example.com/c"]);
    }
}
