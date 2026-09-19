//! Tavily Search API provider for `web_search`.
//!
//! Owns request shaping against Tavily's `POST /search` and the mapping of
//! its JSON envelope onto the provider-neutral [`WebSearchProviderResponse`].
//! Tavily differs from You.com and Brave in ways the gateway adapts here
//! rather than surfacing to the model:
//!
//! - the request is a JSON body, not query parameters, and the API key is
//!   sent only as an `Authorization: Bearer` header, never in the body;
//! - `include_domains` / `exclude_domains` are applied server-side and are
//!   still enforced client-side through [`DomainFilter`] as defense in depth;
//! - `max_results` is capped at [`TAVILY_MAX_RESULTS`] and clamped instead of rejected;
//! - `freshness` maps to `time_range` or a `start_date` / `end_date` pair;
//! - one `topic: "general"` request serves each query, so every hit lands in
//!   the `web` section and `news` stays empty (a second `news` request would
//!   double the credits spent per query);
//! - `country` (Tavily wants full country names, not ISO codes) and the
//!   You.com-specific arguments are dropped.
//!
//! `Accept-Encoding` is deliberately never sent: the core `reqwest` build has no
//! `gzip` feature, so a compressed body could not be decoded.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::args::{DomainFilter, Freshness, WebSearchArguments, clean_string, clean_vec, validate_count};
use super::{
    ApiKey, WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, clean_base_url,
    null_as_default, read_response_limited,
};
use crate::config::WebSearchProviderKind;
use crate::tool::handler::ToolError;
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};

pub(crate) const TAVILY_API_KEY: &str = WebSearchProviderKind::Tavily.default_api_key_env();

/// Largest `max_results` Tavily accepts per request.
pub(crate) const TAVILY_MAX_RESULTS: u8 = 20;

const SEARCH_PATH: &str = "/search";
const DATE_FORMAT: &str = "%Y-%m-%d";
/// One credit per request; `advanced` costs two and is not exposed.
const SEARCH_DEPTH: &str = "basic";
const TOPIC: &str = "general";
/// Tavily's plan-limit statuses, outside the IANA registry.
const PLAN_LIMIT_EXCEEDED: u16 = 432;
const PAYG_LIMIT_EXCEEDED: u16 = 433;

#[derive(Debug, Clone)]
pub(crate) struct TavilySearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: String,
    max_concurrent_requests: NonZeroUsize,
}

impl TavilySearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// counts as unset and fails at execution time; a blank base URL falls back
    /// to Tavily's public endpoint.
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
            .or_else(|| WebSearchProviderKind::Tavily.default_base_url().map(str::to_owned))
            .unwrap_or_default();
        Self {
            client,
            api_key,
            base_url,
            max_concurrent_requests,
        }
    }
}

impl WebSearchProvider for TavilySearchProvider {
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
                .ok_or_else(|| ToolError::Config(format!("{TAVILY_API_KEY} must be set to use the web_search tool")))?;
            let request = TavilySearchRequest::from_args_and_config(query, args, config)?;
            let body = serde_json::to_vec(&request.body)
                .map_err(|e| ToolError::Execution(format!("failed to serialize Tavily search request: {e}")))?;
            let resp = self
                .client
                .post(format!("{}{SEARCH_PATH}", self.base_url))
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .bearer_auth(&api_key.0)
                .body(body)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("Tavily search request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(failure_from_status(resp).await);
            }

            let response_text = read_response_limited(resp, WebSearchProviderKind::Tavily).await?;
            let response: TavilySearchResponse = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("Tavily search returned invalid JSON: {e}")))?;
            Ok(response.into_provider_response(&request.body.query, &request.domain_filter))
        })
    }

    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        Some(self.max_concurrent_requests)
    }
}

/// Maps a non-2xx Tavily response to an actionable, credential-free error.
///
/// `401`/`403` name the key variable without echoing the upstream body; `429`
/// is reported without retrying (a retry loop would blur the gateway tool
/// timeout) and carries the upstream `Retry-After` so the caller can back
/// off; Tavily's `432`/`433` plan-limit statuses are reported without retrying.
async fn failure_from_status(resp: reqwest::Response) -> ToolError {
    let status = resp.status();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ToolError::Execution(format!(
            "Tavily rejected the API key ({status}); check {TAVILY_API_KEY}"
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
                "Tavily rate limited the request ({status}); the gateway does not retry{hint}"
            ))
        }
        _ if matches!(status.as_u16(), PLAN_LIMIT_EXCEEDED | PAYG_LIMIT_EXCEEDED) => ToolError::Execution(format!(
            "Tavily reported the plan usage limit was exceeded ({}); the gateway does not retry",
            status.as_u16()
        )),
        _ => {
            let body = read_response_limited(resp, WebSearchProviderKind::Tavily)
                .await
                .unwrap_or_default();
            ToolError::Execution(format!("Tavily search returned {status}: {body}"))
        }
    }
}

/// Tavily's `POST /search` body, derived from the model's arguments and the
/// request-level tool configuration, plus the client-side [`DomainFilter`]
/// that re-checks Tavily's own domain filtering.
#[derive(Debug, PartialEq, Eq)]
struct TavilySearchRequest {
    body: TavilySearchBody,
    domain_filter: DomainFilter,
}

/// JSON body sent to Tavily. Field order is the serialized order; `None` and
/// empty lists are omitted so the payload only carries what the model asked for.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct TavilySearchBody {
    query: String,
    search_depth: &'static str,
    topic: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_results: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_range: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include_domains: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude_domains: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    safe_search: Option<bool>,
    /// Requested on every search so `page_age` is populated for the
    /// `general` topic too (Tavily enables it automatically only for `news`).
    include_published_date: bool,
}

impl TavilySearchRequest {
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
        let (time_range, start_date, end_date) = args.freshness.map(tavily_freshness).unwrap_or_default();
        let domain_filter = DomainFilter::new(include_domains.as_deref(), exclude_domains.as_deref());

        Ok(Self {
            body: TavilySearchBody {
                query: query.trim().to_owned(),
                search_depth: SEARCH_DEPTH,
                topic: TOPIC,
                max_results,
                time_range,
                start_date,
                end_date,
                include_domains: include_domains.unwrap_or_default(),
                exclude_domains: exclude_domains.unwrap_or_default(),
                language: args.language.as_deref().map(tavily_language),
                safe_search: args.safesearch.as_deref().map(tavily_safe_search),
                include_published_date: true,
            },
            domain_filter,
        })
    }
}

/// Tavily accepts at most [`TAVILY_MAX_RESULTS`] results; the model cannot
/// know provider limits, so a larger request is clamped rather than rejected.
fn clamp_max_results(count: u8) -> u8 {
    if count > TAVILY_MAX_RESULTS {
        tracing::debug!(
            requested = count,
            max = TAVILY_MAX_RESULTS,
            "clamped web_search count to Tavily maximum"
        );
        TAVILY_MAX_RESULTS
    } else {
        count
    }
}

/// `country` is dropped because Tavily expects full lowercase country names
/// rather than the ISO 3166-1 codes the tool contract carries; You.com-specific
/// arguments have no Tavily equivalent and are dropped; `boost_domains` has no
/// filtering semantics, so it is dropped too.
fn log_ignored_arguments(args: &WebSearchArguments, config: &WebSearchToolParam) {
    let config_country = config
        .user_location
        .as_ref()
        .is_some_and(|location| clean_string(location.country.as_deref()).is_some());
    let ignored: Vec<&str> = [
        ("country", config_country || args.country.is_some()),
        ("livecrawl", args.livecrawl.is_some()),
        ("livecrawl_formats", args.livecrawl_formats.is_some()),
        ("crawl_timeout", args.crawl_timeout.is_some()),
        ("boost_domains", args.boost_domains.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    if !ignored.is_empty() {
        tracing::debug!(arguments = ?ignored, "ignored web_search arguments without a Tavily equivalent");
    }
}

/// Renders the typed freshness filter as Tavily's `time_range` or as an
/// explicit `start_date` / `end_date` pair.
///
/// The gateway's `YYYY-MM-DDtoYYYY-MM-DD` range is inclusive, while Tavily
/// documents `start_date` as "results after this date" and `end_date` as
/// "results before this date". Each bound is widened by one day so both
/// boundary dates stay in scope; a date that cannot be widened (the edge of
/// the calendar) is sent as-is.
fn tavily_freshness(freshness: Freshness) -> (Option<&'static str>, Option<String>, Option<String>) {
    match freshness {
        Freshness::Day => (Some("day"), None, None),
        Freshness::Week => (Some("week"), None, None),
        Freshness::Month => (Some("month"), None, None),
        Freshness::Year => (Some("year"), None, None),
        Freshness::Range { from, to } => (
            None,
            Some(from.pred_opt().unwrap_or(from).format(DATE_FORMAT).to_string()),
            Some(to.succ_opt().unwrap_or(to).format(DATE_FORMAT).to_string()),
        ),
    }
}

/// Compound language tags Tavily documents beyond bare ISO 639-1 codes.
const TAVILY_COMPOUND_LANGUAGES: [&str; 1] = ["zh-cn"];

/// Tavily takes an ISO 639-1 code plus the compound tags in
/// [`TAVILY_COMPOUND_LANGUAGES`]. A documented compound tag is sent lowercased
/// (`zh-CN` → `zh-cn`); any other BCP 47 tag such as `en-GB` is reduced to its
/// lowercase primary subtag so a regional variant never fails the request.
fn tavily_language(language: &str) -> String {
    let normalized = language.replace('_', "-").to_ascii_lowercase();
    if TAVILY_COMPOUND_LANGUAGES.contains(&normalized.as_str()) {
        return normalized;
    }
    normalized.split('-').next().map_or(normalized.clone(), str::to_owned)
}

/// Tavily's `safe_search` is boolean: anything but an explicit `off` enables it.
fn tavily_safe_search(safesearch: &str) -> bool {
    !safesearch.eq_ignore_ascii_case("off")
}

/// Tavily's `POST /search` envelope. Only `results`, `response_time`, and
/// `request_id` are modeled; `answer`, `images`, `auto_parameters`, `usage`,
/// and unknown keys are ignored so upstream additions never break the provider.
#[derive(Debug, Default, Deserialize)]
struct TavilySearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    results: Vec<TavilyResult>,
    /// Provider-reported latency in seconds.
    #[serde(default)]
    response_time: Option<f64>,
    #[serde(default)]
    request_id: Option<String>,
}

/// One Tavily hit. `content` is the cleaned snippet; `raw_content`, `score`,
/// `favicon`, `images`, and `id` are not modeled.
#[derive(Debug, Default, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
}

impl From<TavilyResult> for WebSearchResult {
    fn from(result: TavilyResult) -> Self {
        Self {
            url: result.url.trim().to_owned(),
            title: clean_string(result.title.as_deref()),
            description: clean_string(result.content.as_deref()),
            snippets: Vec::new(),
            page_age: clean_string(result.published_date.as_deref()),
            contents: None,
        }
    }
}

impl TavilySearchResponse {
    fn into_provider_response(self, query: &str, domain_filter: &DomainFilter) -> WebSearchProviderResponse {
        let mut web: Vec<WebSearchResult> = self.results.into_iter().map(Into::into).collect();
        domain_filter.retain(&mut web);
        WebSearchProviderResponse {
            web,
            news: Vec::new(),
            metadata: WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Tavily,
                query: query.to_owned(),
                search_uuid: clean_string(self.request_id.as_deref()),
                latency: self.response_time,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::{WebSearchFilters, WebSearchUserLocation};

    fn build_provider(api_key: Option<&str>, base_url: Option<&str>) -> TavilySearchProvider {
        TavilySearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            api_key.map(str::to_owned),
            base_url.map(str::to_owned),
            NonZeroUsize::new(5).unwrap(),
        )
    }

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    fn body_json(request: &TavilySearchRequest) -> serde_json::Value {
        serde_json::to_value(&request.body).unwrap()
    }

    #[test]
    fn provider_debug_is_redacted_and_defaults_base_url() {
        let provider = build_provider(Some("tvly-super-secret"), None);
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("tvly-super-secret"));
        assert!(rendered.contains("ApiKey(<redacted>)"));
        assert_eq!(provider.base_url, "https://api.tavily.com");
        assert_eq!(provider.max_concurrent_requests(), NonZeroUsize::new(5));

        let provider = build_provider(Some("  "), Some(" https://tavily.example/// "));
        assert!(provider.api_key.is_none());
        assert_eq!(provider.base_url, "https://tavily.example");
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
            "invalid tool config: TAVILY_API_KEY must be set to use the web_search tool"
        );
    }

    #[test]
    fn request_renders_every_argument_in_tavily_syntax() {
        let request = TavilySearchRequest::from_args_and_config(
            "  rust async  ",
            &args(
                r#"{"query":"rust async","count":7,"freshness":"week","language":"en-GB","safesearch":"strict","exclude_domains":[" Spam.example ", ""]}"#,
            ),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(
            body_json(&request),
            serde_json::json!({
                "query": "rust async",
                "search_depth": "basic",
                "topic": "general",
                "max_results": 7,
                "time_range": "week",
                "exclude_domains": ["Spam.example"],
                "language": "en",
                "safe_search": true,
                "include_published_date": true
            })
        );
        assert!(!request.domain_filter.allows("https://spam.example/x"));
        assert!(request.domain_filter.allows("https://other.org/x"));
    }

    #[test]
    fn request_omits_optional_fields_when_unset() {
        let request =
            TavilySearchRequest::from_args_and_config("q", &args(r#"{"query":"q"}"#), &WebSearchToolParam::default())
                .unwrap();
        assert_eq!(
            body_json(&request),
            serde_json::json!({
                "query": "q",
                "search_depth": "basic",
                "topic": "general",
                "include_published_date": true
            })
        );
        assert!(request.domain_filter.is_empty());
        assert!(!serde_json::to_string(&request.body).unwrap().contains("api_key"));
    }

    #[test]
    fn request_clamps_count_and_applies_context_size_default() {
        let request = TavilySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","count":50}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(request.body.max_results, Some(TAVILY_MAX_RESULTS));

        let request = TavilySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","count":20}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(request.body.max_results, Some(20));

        let config = WebSearchToolParam {
            search_context_size: Some(WebSearchContextSize::High),
            ..WebSearchToolParam::default()
        };
        let request = TavilySearchRequest::from_args_and_config("q", &args(r#"{"query":"q"}"#), &config).unwrap();
        assert_eq!(
            request.body.max_results.map(u16::from),
            Some(u16::from(WebSearchContextSize::High.default_count()))
        );

        let error = TavilySearchRequest::from_args_and_config(
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
    fn freshness_renders_time_range_or_date_bounds() {
        assert_eq!(tavily_freshness(Freshness::Day), (Some("day"), None, None));
        assert_eq!(tavily_freshness(Freshness::Week), (Some("week"), None, None));
        assert_eq!(tavily_freshness(Freshness::Month), (Some("month"), None, None));
        assert_eq!(tavily_freshness(Freshness::Year), (Some("year"), None, None));
        // Tavily's bounds are exclusive, so the inclusive range is widened by a day on each side.
        let range: Freshness = "2026-01-02to2026-02-03".parse().unwrap();
        assert_eq!(
            tavily_freshness(range),
            (None, Some("2026-01-01".to_owned()), Some("2026-02-04".to_owned()))
        );
        let range: Freshness = "2026-03-01to2026-12-31".parse().unwrap();
        assert_eq!(
            tavily_freshness(range),
            (None, Some("2026-02-28".to_owned()), Some("2027-01-01".to_owned()))
        );

        let request = TavilySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","freshness":"2026-01-02to2026-02-03"}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        let body = body_json(&request);
        assert_eq!(body["start_date"], "2026-01-01");
        assert_eq!(body["end_date"], "2026-02-04");
        assert!(body.get("time_range").is_none());
    }

    #[test]
    fn language_and_safe_search_are_reduced_to_tavily_values() {
        assert_eq!(tavily_language("en-GB"), "en");
        assert_eq!(tavily_language("pt_BR"), "pt");
        assert_eq!(tavily_language("FR"), "fr");
        assert_eq!(tavily_language("zh-CN"), "zh-cn", "documented compound tags are kept");
        assert_eq!(tavily_language("zh_cn"), "zh-cn");
        assert_eq!(
            tavily_language("zh-TW"),
            "zh",
            "undocumented regional tags fall back to the primary subtag"
        );
        assert!(tavily_safe_search("strict"));
        assert!(tavily_safe_search("moderate"));
        assert!(!tavily_safe_search("off"));
        assert!(!tavily_safe_search("OFF"));
    }

    #[test]
    fn request_ignores_arguments_without_a_tavily_equivalent() {
        let request = TavilySearchRequest::from_args_and_config(
            "q",
            &args(
                r#"{"query":"q","country":"gb","livecrawl":"web","livecrawl_formats":["markdown"],"crawl_timeout":5}"#,
            ),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        let body = body_json(&request);
        for key in [
            "country",
            "livecrawl",
            "livecrawl_formats",
            "crawl_timeout",
            "boost_domains",
        ] {
            assert!(body.get(key).is_none(), "{key} must not be forwarded");
        }
    }

    #[test]
    fn request_prefers_tool_config_filters_over_arguments() {
        let config = WebSearchToolParam {
            filters: Some(WebSearchFilters {
                allowed_domains: Some(vec![" rust-lang.org ".to_owned()]),
                blocked_domains: None,
            }),
            user_location: Some(WebSearchUserLocation {
                country: Some("us".to_owned()),
                ..WebSearchUserLocation::default()
            }),
            ..WebSearchToolParam::default()
        };
        let request = TavilySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","include_domains":["example.com"]}"#),
            &config,
        )
        .unwrap();
        assert_eq!(
            body_json(&request)["include_domains"],
            serde_json::json!(["rust-lang.org"])
        );
        assert!(body_json(&request).get("country").is_none());
        assert!(request.domain_filter.allows("https://doc.rust-lang.org/book"));
        assert!(!request.domain_filter.allows("https://example.com/"));
    }

    #[test]
    fn request_rejects_conflicting_domain_lists() {
        let error = TavilySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","include_domains":["a.com"],"exclude_domains":["b.com"]}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains"
        );
        let error = TavilySearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","include_domains":["a.com"],"boost_domains":["b.com"]}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn response_maps_results_and_tolerates_unknown_fields() {
        let response: TavilySearchResponse = serde_json::from_str(
            r#"{
                "query": "rust",
                "answer": null,
                "images": [],
                "results": [
                    {
                        "title": " Rust ",
                        "url": " https://www.rust-lang.org/ ",
                        "content": "A language",
                        "score": 0.98,
                        "raw_content": "<html>",
                        "published_date": "2026-01-02",
                        "favicon": "https://www.rust-lang.org/favicon.ico",
                        "id": "res_1"
                    },
                    {"url": "https://example.com/no-title", "title": "", "content": null, "published_date": null}
                ],
                "auto_parameters": {"topic": "general"},
                "response_time": 1.25,
                "request_id": "req_123"
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
                    snippets: Vec::new(),
                    page_age: Some("2026-01-02".to_owned()),
                    contents: None,
                },
                WebSearchResult {
                    url: "https://example.com/no-title".to_owned(),
                    ..WebSearchResult::default()
                },
            ]
        );
        assert!(response.news.is_empty());
        assert_eq!(
            response.metadata,
            WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Tavily,
                query: "rust".to_owned(),
                search_uuid: Some("req_123".to_owned()),
                latency: Some(1.25),
            }
        );
        assert_eq!(
            serde_json::to_string(&response.metadata).unwrap(),
            r#"{"provider":"tavily","query":"rust","search_uuid":"req_123","latency":1.25}"#
        );
    }

    #[test]
    fn response_tolerates_missing_and_null_fields() {
        for body in ["{}", r#"{"results":null,"response_time":null,"request_id":null}"#] {
            let response: TavilySearchResponse = serde_json::from_str(body).unwrap();
            let response = response.into_provider_response("q", &DomainFilter::default());
            assert!(response.web.is_empty());
            assert!(response.news.is_empty());
            assert_eq!(
                serde_json::to_string(&response.metadata).unwrap(),
                r#"{"provider":"tavily","query":"q"}"#
            );
        }
    }

    #[test]
    fn response_applies_domain_filter_as_defense_in_depth() {
        let response: TavilySearchResponse = serde_json::from_str(
            r#"{"results": [
                {"url": "https://docs.example.com/a"},
                {"url": "https://notexample.com/b"},
                {"url": "https://EXAMPLE.COM./c"},
                {"url": "not a url"}
            ]}"#,
        )
        .unwrap();
        let filter = DomainFilter::new(Some(&["example.com".to_owned()]), None);
        let response = response.into_provider_response("q", &filter);
        let urls: Vec<_> = response.web.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://docs.example.com/a", "https://EXAMPLE.COM./c"]);
    }
}
