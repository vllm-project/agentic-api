//! SearXNG provider for `web_search`.
//!
//! Owns request shaping against a self-hosted SearXNG instance's
//! `GET /search?format=json` and the mapping of its JSON envelope onto the
//! provider-neutral [`WebSearchProviderResponse`]. SearXNG is a keyless
//! metasearch engine, so it differs from You.com and Brave in ways the gateway
//! adapts here rather than surfacing to the model:
//!
//! - the endpoint is mandatory: there is no public SearXNG API, so a missing
//!   base URL is a configuration error;
//! - web and news hits arrive in one `results[]` list, split by each hit's
//!   `category` (`categories=general,news` is requested once per query);
//! - there is no server-side domain filter, so `include_domains` /
//!   `exclude_domains` are applied client-side through [`DomainFilter`];
//! - there is no `count` parameter, so results are truncated client-side after
//!   filtering (paging through `pageno` is out of scope);
//! - `freshness` maps onto `time_range=day|week|month|year`; a date range has
//!   no SearXNG equivalent and is ignored;
//! - `language` must match SearXNG's `xx` / `xx-YY` shape or the instance
//!   answers `400`, so BCP 47 tags are normalized before sending;
//! - `safesearch` is an integer (`0`, `1`, `2`) rather than a name.
//!
//! `Accept-Encoding` is deliberately never sent: the core `reqwest` build has no
//! `gzip` feature, so a compressed body could not be decoded. SearXNG's optional
//! bot-detection limiter (`server.limiter: true`) rejects such requests with
//! `429`, which the error message explains to the operator.

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
use crate::error::Error;
use crate::tool::handler::ToolError;
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};

pub(crate) const SEARXNG_API_KEY: &str = WebSearchProviderKind::Searxng.default_api_key_env();

/// Operator-facing fix for a missing SearXNG endpoint, shared by the startup
/// check in `agentic-server` and the execution-time fallback here.
pub const SEARXNG_BASE_URL_HINT: &str = "SearXNG requires a base URL; set AGENTIC_WEB_SEARCH_BASE_URL or [web_search] base_url \
     (for example http://searxng:8080)";

const SEARCH_PATH: &str = "search";
const CATEGORIES: &str = "general,news";

/// Checks that a configured SearXNG endpoint can be addressed: an absolute
/// `http`/`https` URL with a host and no query or fragment, because the
/// provider appends `/search` to its path. A blank or missing value is
/// reported with [`SEARXNG_BASE_URL_HINT`]. Shared by the `agentic-server`
/// startup check and the provider so both reject the same inputs.
///
/// # Errors
///
/// Returns [`Error::Config`] with the operator-facing message when the value
/// is blank, not an absolute `http(s)` URL with a host, or carries a query or
/// fragment.
pub fn validate_searxng_base_url(value: Option<&str>) -> Result<(), Error> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    let Some(value) = value else {
        return Err(Error::Config(SEARXNG_BASE_URL_HINT.to_owned()));
    };
    parse_base_url(value).map(drop).map_err(Error::Config)
}

/// Parses a non-blank endpoint, producing the operator-facing message on failure.
fn parse_base_url(value: &str) -> Result<url::Url, String> {
    match url::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {
            if url.query().is_some() || url.fragment().is_some() {
                return Err(format!(
                    "SearXNG base URL {value:?} must not contain a query or fragment; the gateway appends /search to \
                     its path"
                ));
            }
            Ok(url)
        }
        _ => Err(format!(
            "SearXNG base URL {value:?} must be an absolute http(s) URL such as http://searxng:8080"
        )),
    }
}

/// Builds the `/search` endpoint under the configured base path, so a
/// sub-path mount such as `http://host/searxng` resolves to
/// `http://host/searxng/search`.
fn search_endpoint(base_url: &str) -> Result<url::Url, ToolError> {
    let mut url = parse_base_url(base_url).map_err(ToolError::Config)?;
    let path = format!("{}/{SEARCH_PATH}", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}
const NEWS_CATEGORY: &str = "news";

#[derive(Debug, Clone)]
pub(crate) struct SearxngSearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: Option<String>,
    max_concurrent_requests: NonZeroUsize,
}

impl SearxngSearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// counts as unset (SearXNG needs none); a blank base URL counts as unset
    /// and fails at execution time because SearXNG has no default endpoint.
    pub(crate) fn from_values(
        client: Arc<reqwest::Client>,
        api_key: Option<String>,
        base_url: Option<String>,
        max_concurrent_requests: NonZeroUsize,
    ) -> Self {
        Self {
            client,
            api_key: api_key
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .map(ApiKey),
            base_url: base_url.and_then(|value| clean_base_url(&value)),
            max_concurrent_requests,
        }
    }
}

impl WebSearchProvider for SearxngSearchProvider {
    fn search<'a>(
        &'a self,
        query: &'a str,
        args: &'a WebSearchArguments,
        config: &'a WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let base_url = self
                .base_url
                .as_deref()
                .ok_or_else(|| ToolError::Config(SEARXNG_BASE_URL_HINT.to_owned()))?;
            let endpoint = search_endpoint(base_url)?;
            let request = SearxngSearchRequest::from_args_and_config(query, args, config)?;
            let mut builder = self
                .client
                .get(endpoint)
                .query(&request.query_params())
                .header("Accept", "application/json");
            if let Some(api_key) = &self.api_key {
                builder = builder.bearer_auth(&api_key.0);
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("SearXNG request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(failure_from_status(resp).await);
            }

            let response_text = read_response_limited(resp, WebSearchProviderKind::Searxng).await?;
            let response: SearxngSearchResponse = serde_json::from_str(&response_text).map_err(|e| {
                ToolError::Execution(format!(
                    "SearXNG returned a non-JSON response ({e}); confirm base_url points at a SearXNG instance \
                     and that `search.formats` in settings.yml includes `json`"
                ))
            })?;
            Ok(response.into_provider_response(&request))
        })
    }

    fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
        Some(self.max_concurrent_requests)
    }
}

/// Maps a non-2xx SearXNG response to an actionable, credential-free error.
///
/// SearXNG answers `403` when `format=json` is not enabled, so that status is
/// a configuration hint before it is an authentication one. `429` is what the
/// bot-detection limiter returns for a request without `Accept-Encoding:
/// gzip`, which this client cannot send; it is reported without retrying.
async fn failure_from_status(resp: reqwest::Response) -> ToolError {
    let status = resp.status();
    match status {
        StatusCode::FORBIDDEN => ToolError::Execution(format!(
            "SearXNG refused the request ({status}); enable the JSON format with `search.formats: [html, json]` \
             in settings.yml, or check {SEARXNG_API_KEY} if the instance is behind an authenticating proxy"
        )),
        StatusCode::UNAUTHORIZED => ToolError::Execution(format!(
            "SearXNG rejected the credential ({status}); check {SEARXNG_API_KEY}"
        )),
        StatusCode::TOO_MANY_REQUESTS => {
            let hint = resp
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map_or_else(String::new, |value| format!("; retry after {value}"));
            ToolError::Execution(format!(
                "SearXNG rate limited the request ({status}); the gateway does not retry{hint}. If the instance runs \
                 with `server.limiter: true`, its bot detection blocks the gateway (which cannot send \
                 Accept-Encoding: gzip): add the gateway address to `botdetection.ip_lists.pass_ip` in limiter.toml \
                 or disable the limiter"
            ))
        }
        StatusCode::BAD_REQUEST => {
            let body = read_response_limited(resp, WebSearchProviderKind::Searxng)
                .await
                .unwrap_or_default();
            ToolError::Execution(format!("SearXNG rejected a search parameter ({status}): {body}"))
        }
        _ => {
            let body = read_response_limited(resp, WebSearchProviderKind::Searxng)
                .await
                .unwrap_or_default();
            ToolError::Execution(format!("SearXNG returned {status}: {body}"))
        }
    }
}

/// Query parameters for SearXNG's `GET /search`, derived from the model's
/// arguments and the request-level tool configuration, plus the client-side
/// adaptations SearXNG cannot apply itself.
#[derive(Debug, PartialEq, Eq)]
struct SearxngSearchRequest {
    query: String,
    /// Per-section result ceiling applied after filtering.
    count: Option<u8>,
    time_range: Option<&'static str>,
    language: Option<String>,
    safesearch: Option<u8>,
    domain_filter: DomainFilter,
}

impl SearxngSearchRequest {
    fn query_params(&self) -> Vec<(String, String)> {
        let mut params = vec![
            ("q".to_owned(), self.query.clone()),
            ("format".to_owned(), "json".to_owned()),
            ("categories".to_owned(), CATEGORIES.to_owned()),
        ];
        if let Some(time_range) = self.time_range {
            params.push(("time_range".to_owned(), time_range.to_owned()));
        }
        if let Some(language) = &self.language {
            params.push(("language".to_owned(), language.clone()));
        }
        if let Some(safesearch) = self.safesearch {
            params.push(("safesearch".to_owned(), safesearch.to_string()));
        }
        params
    }

    fn from_args_and_config(
        query: &str,
        args: &WebSearchArguments,
        config: &WebSearchToolParam,
    ) -> Result<Self, ToolError> {
        let count = args
            .count
            .or_else(|| {
                config
                    .search_context_size
                    .map(WebSearchContextSize::default_count)
                    .map(u16::from)
            })
            .map(validate_count)
            .transpose()?;
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

        Ok(Self {
            query: query.trim().to_owned(),
            count,
            time_range: args.freshness.and_then(searxng_time_range),
            language: args.language.as_deref().and_then(searxng_language),
            safesearch: args.safesearch.as_deref().and_then(searxng_safesearch),
            domain_filter: DomainFilter::new(include_domains.as_deref(), exclude_domains.as_deref()),
        })
    }
}

/// Arguments without a SearXNG equivalent are dropped: the You.com-specific
/// crawl controls, `boost_domains` (no filtering semantics), and `country`
/// (SearXNG localizes through `language` only).
fn log_ignored_arguments(args: &WebSearchArguments, config: &WebSearchToolParam) {
    let country = args.country.is_some()
        || config
            .user_location
            .as_ref()
            .is_some_and(|location| clean_string(location.country.as_deref()).is_some());
    let ignored: Vec<&str> = [
        ("livecrawl", args.livecrawl.is_some()),
        ("livecrawl_formats", args.livecrawl_formats.is_some()),
        ("crawl_timeout", args.crawl_timeout.is_some()),
        ("boost_domains", args.boost_domains.is_some()),
        ("country", country),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    if !ignored.is_empty() {
        tracing::debug!(arguments = ?ignored, "ignored web_search arguments without a SearXNG equivalent");
    }
}

/// Renders the typed freshness filter as SearXNG's `time_range`. SearXNG has no
/// date-range filter, so a range is dropped rather than approximated.
fn searxng_time_range(freshness: Freshness) -> Option<&'static str> {
    match freshness {
        Freshness::Day => Some("day"),
        Freshness::Week => Some("week"),
        Freshness::Month => Some("month"),
        Freshness::Year => Some("year"),
        Freshness::Range { .. } => {
            tracing::debug!("ignored web_search freshness date range; SearXNG supports day/week/month/year only");
            None
        }
    }
}

/// Normalizes a BCP 47 tag to the `xx` / `xx-YY` shape SearXNG validates
/// (`^[a-z]{2,3}(-[a-zA-Z]{2})?$`); `auto` and `all` pass through. Script and
/// variant subtags would make SearXNG answer `400`, so they are dropped, and a
/// tag with no usable primary subtag is ignored.
fn searxng_language(value: &str) -> Option<String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") || value.eq_ignore_ascii_case("all") {
        return Some(value.to_ascii_lowercase());
    }
    let mut subtags = value.split(['-', '_']);
    let primary = subtags.next()?.to_ascii_lowercase();
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|byte| byte.is_ascii_lowercase()) {
        tracing::debug!(
            language = value,
            "ignored web_search language that SearXNG cannot parse"
        );
        return None;
    }
    let region = subtags.find(|subtag| subtag.len() == 2 && subtag.bytes().all(|byte| byte.is_ascii_alphabetic()));
    Some(match region {
        Some(region) => format!("{primary}-{}", region.to_ascii_uppercase()),
        None => primary,
    })
}

/// Maps the named `safesearch` levels onto SearXNG's `0` / `1` / `2`.
fn searxng_safesearch(value: &str) -> Option<u8> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" => Some(0),
        "moderate" | "1" => Some(1),
        "strict" | "2" => Some(2),
        other => {
            tracing::debug!(
                safesearch = other,
                "ignored web_search safesearch level unknown to SearXNG"
            );
            None
        }
    }
}

/// SearXNG's `format=json` envelope. Only `results` is modeled; `answers`,
/// `infoboxes`, `suggestions`, `corrections`, `unresponsive_engines`, and
/// unknown keys are ignored so upstream additions never break the provider.
#[derive(Debug, Default, Deserialize)]
struct SearxngSearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    results: Vec<SearxngResult>,
}

/// One SearXNG hit. `content` is the snippet, `publishedDate` the ISO
/// timestamp SearXNG serializes for dated results, and `pubdate` its
/// preformatted form; ranking and cosmetic fields (`engine`, `engines`,
/// `score`, `positions`, `thumbnail`, `parsed_url`) are not modeled.
#[derive(Debug, Default, Deserialize)]
struct SearxngResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default, rename = "publishedDate")]
    published_date: Option<String>,
    #[serde(default)]
    pubdate: Option<String>,
}

impl SearxngResult {
    fn is_news(&self) -> bool {
        self.category
            .as_deref()
            .is_some_and(|category| category.trim().eq_ignore_ascii_case(NEWS_CATEGORY))
    }
}

impl From<SearxngResult> for WebSearchResult {
    fn from(result: SearxngResult) -> Self {
        Self {
            url: result.url.trim().to_owned(),
            title: clean_string(result.title.as_deref()),
            description: clean_string(result.content.as_deref()),
            snippets: Vec::new(),
            page_age: clean_string(result.published_date.as_deref())
                .or_else(|| clean_string(result.pubdate.as_deref())),
            contents: None,
        }
    }
}

impl SearxngSearchResponse {
    fn into_provider_response(self, request: &SearxngSearchRequest) -> WebSearchProviderResponse {
        let (news, web): (Vec<SearxngResult>, Vec<SearxngResult>) =
            self.results.into_iter().partition(SearxngResult::is_news);
        let mut web: Vec<WebSearchResult> = web.into_iter().map(Into::into).collect();
        let mut news: Vec<WebSearchResult> = news.into_iter().map(Into::into).collect();
        request.domain_filter.retain(&mut web);
        request.domain_filter.retain(&mut news);
        if let Some(count) = request.count {
            web.truncate(usize::from(count));
            news.truncate(usize::from(count));
        }
        WebSearchProviderResponse {
            web,
            news,
            metadata: WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Searxng,
                query: request.query.clone(),
                search_uuid: None,
                latency: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::{WebSearchFilters, WebSearchUserLocation};

    fn build_provider(api_key: Option<&str>, base_url: Option<&str>) -> SearxngSearchProvider {
        SearxngSearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            api_key.map(str::to_owned),
            base_url.map(str::to_owned),
            NonZeroUsize::new(3).unwrap(),
        )
    }

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    fn request(json: &str, config: &WebSearchToolParam) -> SearxngSearchRequest {
        SearxngSearchRequest::from_args_and_config("q", &args(json), config).unwrap()
    }

    #[test]
    fn provider_debug_is_redacted_and_base_url_has_no_default() {
        let provider = build_provider(Some("super-secret-key"), Some(" http://searxng:8080/// "));
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("super-secret-key"));
        assert!(rendered.contains("ApiKey(<redacted>)"));
        assert_eq!(provider.base_url.as_deref(), Some("http://searxng:8080"));
        assert_eq!(provider.max_concurrent_requests(), NonZeroUsize::new(3));

        let provider = build_provider(Some("  "), Some("  "));
        assert!(provider.api_key.is_none());
        assert!(provider.base_url.is_none());
        assert!(build_provider(None, None).base_url.is_none());
    }

    #[test]
    fn search_endpoint_appends_search_under_the_base_path() {
        assert_eq!(
            search_endpoint("http://searxng:8080").unwrap().as_str(),
            "http://searxng:8080/search"
        );
        assert_eq!(
            search_endpoint("https://search.internal/searxng").unwrap().as_str(),
            "https://search.internal/searxng/search"
        );
        assert_eq!(
            search_endpoint("http://[::1]:8080/a/b").unwrap().as_str(),
            "http://[::1]:8080/a/b/search"
        );
        for invalid in [
            "searxng:8080",
            "ftp://searxng",
            "http://",
            "http://host?x=y",
            "http://host/#top",
        ] {
            let error = search_endpoint(invalid).expect_err(invalid).to_string();
            assert!(error.starts_with("invalid tool config: SearXNG base URL"), "{error}");
        }
    }

    #[test]
    fn validate_base_url_shares_the_provider_rules() {
        assert!(validate_searxng_base_url(Some(" http://searxng:8080/ ")).is_ok());
        assert_eq!(
            validate_searxng_base_url(None).unwrap_err().to_string(),
            SEARXNG_BASE_URL_HINT
        );
        assert_eq!(
            validate_searxng_base_url(Some("  ")).unwrap_err().to_string(),
            SEARXNG_BASE_URL_HINT
        );
        assert_eq!(
            validate_searxng_base_url(Some("http://host?x=y"))
                .unwrap_err()
                .to_string(),
            "SearXNG base URL \"http://host?x=y\" must not contain a query or fragment; the gateway appends /search \
             to its path"
        );
        assert_eq!(
            validate_searxng_base_url(Some("/searxng")).unwrap_err().to_string(),
            "SearXNG base URL \"/searxng\" must be an absolute http(s) URL such as http://searxng:8080"
        );
    }

    #[tokio::test]
    async fn search_without_base_url_names_the_setting() {
        let provider = build_provider(None, None);
        let error = provider
            .search("q", &args(r#"{"query":"q"}"#), &WebSearchToolParam::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("invalid tool config: {SEARXNG_BASE_URL_HINT}")
        );
    }

    #[test]
    fn request_renders_every_argument_in_searxng_syntax() {
        let request = SearxngSearchRequest::from_args_and_config(
            "  rust async  ",
            &args(r#"{"query":"rust async","count":7,"freshness":"week","language":"en-GB","safesearch":"strict"}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap();
        assert_eq!(
            request.query_params(),
            [
                ("q", "rust async"),
                ("format", "json"),
                ("categories", "general,news"),
                ("time_range", "week"),
                ("language", "en-GB"),
                ("safesearch", "2"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        assert_eq!(request.count, Some(7));
        assert!(request.domain_filter.is_empty());
    }

    #[test]
    fn request_applies_context_size_default_and_validates_count() {
        let config = WebSearchToolParam {
            search_context_size: Some(WebSearchContextSize::Low),
            ..WebSearchToolParam::default()
        };
        assert_eq!(
            request(r#"{"query":"q"}"#, &config).count.map(u16::from),
            Some(u16::from(WebSearchContextSize::Low.default_count()))
        );
        assert_eq!(request(r#"{"query":"q"}"#, &WebSearchToolParam::default()).count, None);
        assert_eq!(
            request(r#"{"query":"q","count":100}"#, &WebSearchToolParam::default()).count,
            Some(100)
        );
        let error = SearxngSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","count":101}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_search count must be between 1 and 100"
        );
    }

    #[test]
    fn freshness_maps_named_ranges_and_drops_date_ranges() {
        assert_eq!(searxng_time_range(Freshness::Day), Some("day"));
        assert_eq!(searxng_time_range(Freshness::Week), Some("week"));
        assert_eq!(searxng_time_range(Freshness::Month), Some("month"));
        assert_eq!(searxng_time_range(Freshness::Year), Some("year"));
        let range: Freshness = "2026-01-02to2026-02-03".parse().unwrap();
        assert_eq!(searxng_time_range(range), None);
    }

    #[test]
    fn language_is_normalized_to_searxng_shape() {
        assert_eq!(searxng_language("en").as_deref(), Some("en"));
        assert_eq!(searxng_language(" EN-gb ").as_deref(), Some("en-GB"));
        assert_eq!(searxng_language("pt_BR").as_deref(), Some("pt-BR"));
        assert_eq!(searxng_language("zh-Hans").as_deref(), Some("zh"));
        assert_eq!(searxng_language("zh-Hant-TW").as_deref(), Some("zh-TW"));
        assert_eq!(searxng_language("ast").as_deref(), Some("ast"));
        assert_eq!(searxng_language("Auto").as_deref(), Some("auto"));
        assert_eq!(searxng_language("all").as_deref(), Some("all"));
        assert_eq!(searxng_language("x"), None);
        assert_eq!(searxng_language("english"), None);
        assert_eq!(searxng_language("e1"), None);
    }

    #[test]
    fn safesearch_maps_named_levels_and_digits() {
        assert_eq!(searxng_safesearch("off"), Some(0));
        assert_eq!(searxng_safesearch(" Moderate "), Some(1));
        assert_eq!(searxng_safesearch("strict"), Some(2));
        assert_eq!(searxng_safesearch("1"), Some(1));
        assert_eq!(searxng_safesearch("extreme"), None);
    }

    #[test]
    fn request_ignores_unsupported_arguments_and_builds_domain_filter() {
        let config = WebSearchToolParam {
            user_location: Some(WebSearchUserLocation {
                country: Some("us".to_owned()),
                ..WebSearchUserLocation::default()
            }),
            ..WebSearchToolParam::default()
        };
        let request = request(
            r#"{"query":"q","country":"de","livecrawl":"web","livecrawl_formats":["markdown"],"crawl_timeout":5,"boost_domains":["x.org"],"freshness":"2026-01-02to2026-02-03","language":"english","safesearch":"extreme","exclude_domains":["Example.com"]}"#,
            &config,
        );
        assert_eq!(
            request.query_params(),
            [("q", "q"), ("format", "json"), ("categories", "general,news")]
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        assert!(!request.domain_filter.allows("https://docs.example.com/x"));
        assert!(request.domain_filter.allows("https://other.org/x"));
    }

    #[test]
    fn request_prefers_tool_config_filters_and_rejects_conflicting_lists() {
        let config = WebSearchToolParam {
            filters: Some(WebSearchFilters {
                allowed_domains: Some(vec!["rust-lang.org".to_owned()]),
                blocked_domains: None,
            }),
            ..WebSearchToolParam::default()
        };
        let request = request(r#"{"query":"q","include_domains":["example.com"]}"#, &config);
        assert!(request.domain_filter.allows("https://doc.rust-lang.org/book"));
        assert!(!request.domain_filter.allows("https://example.com/"));

        let error = SearxngSearchRequest::from_args_and_config(
            "q",
            &args(r#"{"query":"q","include_domains":["a.com"],"exclude_domains":["b.com"]}"#),
            &WebSearchToolParam::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains"
        );
    }

    #[test]
    fn response_splits_sections_by_category_and_tolerates_unknown_fields() {
        let response: SearxngSearchResponse = serde_json::from_str(
            r#"{
                "query": "rust",
                "number_of_results": 0,
                "answers": [], "corrections": [], "infoboxes": [], "suggestions": [],
                "unresponsive_engines": [["bing", "timeout"]],
                "results": [
                    {
                        "url": " https://www.rust-lang.org/ ",
                        "title": " Rust ",
                        "content": "A language",
                        "engine": "duckduckgo",
                        "engines": ["duckduckgo", "google"],
                        "parsed_url": ["https", "www.rust-lang.org", "/", "", "", ""],
                        "template": "default.html",
                        "positions": [1, 2],
                        "score": 3.5,
                        "category": "general",
                        "thumbnail": ""
                    },
                    {"url": "https://example.com/no-title", "title": "", "content": null, "category": "general"},
                    {
                        "url": "https://blog.rust-lang.org/1",
                        "title": "Release",
                        "content": "Rust 1.99 is out",
                        "category": "news",
                        "publishedDate": "2026-09-01T10:00:00",
                        "pubdate": "2026-09-01 10:00:00"
                    },
                    {"url": "https://news.example.com/2", "title": "Dated", "category": "News", "pubdate": "2026-09-02 08:00:00"},
                    {"url": "https://uncategorized.example.com/3"}
                ]
            }"#,
        )
        .unwrap();
        let request = request(r#"{"query":"rust"}"#, &WebSearchToolParam::default());
        let response = response.into_provider_response(&request);
        assert_eq!(
            response.web,
            vec![
                WebSearchResult {
                    url: "https://www.rust-lang.org/".to_owned(),
                    title: Some("Rust".to_owned()),
                    description: Some("A language".to_owned()),
                    ..WebSearchResult::default()
                },
                WebSearchResult {
                    url: "https://example.com/no-title".to_owned(),
                    ..WebSearchResult::default()
                },
                WebSearchResult {
                    url: "https://uncategorized.example.com/3".to_owned(),
                    ..WebSearchResult::default()
                },
            ]
        );
        assert_eq!(
            response.news,
            vec![
                WebSearchResult {
                    url: "https://blog.rust-lang.org/1".to_owned(),
                    title: Some("Release".to_owned()),
                    description: Some("Rust 1.99 is out".to_owned()),
                    page_age: Some("2026-09-01T10:00:00".to_owned()),
                    ..WebSearchResult::default()
                },
                WebSearchResult {
                    url: "https://news.example.com/2".to_owned(),
                    title: Some("Dated".to_owned()),
                    page_age: Some("2026-09-02 08:00:00".to_owned()),
                    ..WebSearchResult::default()
                },
            ]
        );
        assert_eq!(
            response.metadata,
            WebSearchProviderMetadata {
                provider: WebSearchProviderKind::Searxng,
                query: "q".to_owned(),
                search_uuid: None,
                latency: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&response.metadata).unwrap(),
            r#"{"provider":"searxng","query":"q"}"#
        );
    }

    #[test]
    fn response_tolerates_missing_and_null_results() {
        let request = request(r#"{"query":"q"}"#, &WebSearchToolParam::default());
        for body in ["{}", r#"{"results":null}"#, r#"{"results":[]}"#] {
            let response: SearxngSearchResponse = serde_json::from_str(body).unwrap();
            let response = response.into_provider_response(&request);
            assert!(response.web.is_empty());
            assert!(response.news.is_empty());
        }
    }

    #[test]
    fn response_filters_domains_then_truncates_to_count() {
        let response: SearxngSearchResponse = serde_json::from_str(
            r#"{"results": [
                {"url": "https://docs.example.com/a"},
                {"url": "https://notexample.com/b"},
                {"url": "https://EXAMPLE.COM./c"},
                {"url": "not a url"},
                {"url": "https://api.example.com/d"},
                {"url": "https://news.example.com/e", "category": "news"},
                {"url": "https://other.org/f", "category": "news"},
                {"url": "https://feed.example.com/g", "category": "news"}
            ]}"#,
        )
        .unwrap();
        let request = request(
            r#"{"query":"q","count":2,"include_domains":["example.com"]}"#,
            &WebSearchToolParam::default(),
        );
        let response = response.into_provider_response(&request);
        let urls = |results: &[WebSearchResult]| results.iter().map(|r| r.url.clone()).collect::<Vec<_>>();
        assert_eq!(
            urls(&response.web),
            ["https://docs.example.com/a", "https://EXAMPLE.COM./c"]
        );
        assert_eq!(
            urls(&response.news),
            ["https://news.example.com/e", "https://feed.example.com/g"]
        );
    }
}
