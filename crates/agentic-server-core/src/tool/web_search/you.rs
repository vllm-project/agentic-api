//! You.com Search API provider for `web_search`.
//!
//! Owns request shaping against You.com's `GET /v1/search` and the mapping of
//! its JSON envelope onto the provider-neutral [`WebSearchProviderResponse`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::args::{Freshness, WebSearchArguments, clean_string, clean_vec, validate_count};
use super::{
    WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, null_as_default,
    read_response_limited,
};
use crate::config::WebSearchProviderKind;
use crate::tool::handler::ToolError;
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};

pub(crate) const YOU_API_KEY: &str = WebSearchProviderKind::You.default_api_key_env();
pub(crate) const YOU_API_BASE_URL: &str = "YOU_API_BASE_URL";

/// Provider credential whose `Debug` output never contains the secret.
#[derive(Clone)]
pub(crate) struct ApiKey(pub String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct YouSearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<ApiKey>,
    base_url: Option<String>,
}

impl YouSearchProvider {
    /// Builds a provider from optional environment-style values: a blank key
    /// or base URL counts as unset and fails at execution time.
    pub(crate) fn from_values(client: Arc<reqwest::Client>, api_key: Option<String>, base_url: Option<String>) -> Self {
        let api_key = api_key
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(ApiKey);
        let base_url = base_url.and_then(|value| clean_base_url(&value));
        Self {
            client,
            api_key,
            base_url,
        }
    }

    pub(crate) fn with_api_key(client: Arc<reqwest::Client>, api_key: String, base_url: &str) -> Self {
        Self {
            client,
            api_key: Some(ApiKey(api_key)),
            base_url: clean_base_url(base_url),
        }
    }
}

impl WebSearchProvider for YouSearchProvider {
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
                .ok_or_else(|| ToolError::Config(format!("{YOU_API_KEY} must be set to use the web_search tool")))?;
            let base_url = self.base_url.as_deref().ok_or_else(|| {
                ToolError::Config(format!("{YOU_API_BASE_URL} must be set to use the web_search tool"))
            })?;
            let request = YouSearchRequest::from_args_and_config(query, args, config)?;
            let resp = self
                .client
                .get(format!("{base_url}/v1/search"))
                .query(&request.query_params())
                .header("X-API-Key", &api_key.0)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("You.com search request failed: {e}")))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = read_response_limited(resp, WebSearchProviderKind::You)
                    .await
                    .unwrap_or_default();
                return Err(ToolError::Execution(format!(
                    "You.com search returned {status}: {body}"
                )));
            }

            let response_text = read_response_limited(resp, WebSearchProviderKind::You).await?;
            let response: YouSearchResponse = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("You.com search returned invalid JSON: {e}")))?;
            Ok(response.into_provider_response(&request.query))
        })
    }
}

/// Query parameters for You.com's `GET /v1/search`, derived from the model's
/// arguments and the request-level tool configuration.
#[derive(Debug, PartialEq, Eq)]
struct YouSearchRequest {
    query: String,
    count: Option<u8>,
    freshness: Option<Freshness>,
    country: Option<String>,
    language: Option<String>,
    safesearch: Option<String>,
    livecrawl: Option<String>,
    livecrawl_formats: Option<Vec<String>>,
    crawl_timeout: Option<u8>,
    include_domains: Option<Vec<String>>,
    exclude_domains: Option<Vec<String>>,
    boost_domains: Option<Vec<String>>,
}

impl YouSearchRequest {
    fn query_params(&self) -> Vec<(String, String)> {
        let mut params = vec![("query".to_owned(), self.query.clone())];
        if let Some(count) = self.count {
            params.push(("count".to_owned(), count.to_string()));
        }
        if let Some(freshness) = &self.freshness {
            params.push(("freshness".to_owned(), freshness.to_string()));
        }
        if let Some(country) = &self.country {
            params.push(("country".to_owned(), country.clone()));
        }
        if let Some(language) = &self.language {
            params.push(("language".to_owned(), language.clone()));
        }
        if let Some(safesearch) = &self.safesearch {
            params.push(("safesearch".to_owned(), safesearch.clone()));
        }
        if let Some(livecrawl) = &self.livecrawl {
            params.push(("livecrawl".to_owned(), livecrawl.clone()));
        }
        for format in self.livecrawl_formats.iter().flatten() {
            params.push(("livecrawl_formats".to_owned(), format.clone()));
        }
        if let Some(crawl_timeout) = self.crawl_timeout {
            params.push(("crawl_timeout".to_owned(), crawl_timeout.to_string()));
        }
        for domain in self.include_domains.iter().flatten() {
            params.push(("include_domains".to_owned(), domain.clone()));
        }
        for domain in self.exclude_domains.iter().flatten() {
            params.push(("exclude_domains".to_owned(), domain.clone()));
        }
        for domain in self.boost_domains.iter().flatten() {
            params.push(("boost_domains".to_owned(), domain.clone()));
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
        let crawl_timeout = args.crawl_timeout.map(validate_crawl_timeout).transpose()?;
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
        let boost_domains = args.boost_domains.clone();
        if include_domains.is_some() && (exclude_domains.is_some() || boost_domains.is_some()) {
            return Err(ToolError::Config(
                "include_domains cannot be combined with exclude_domains or boost_domains".to_owned(),
            ));
        }
        let country = config
            .user_location
            .as_ref()
            .and_then(|location| clean_string(location.country.as_deref()))
            .or_else(|| args.country.clone())
            .map(|value| value.to_ascii_uppercase());

        Ok(Self {
            query: query.trim().to_owned(),
            count,
            freshness: args.freshness,
            country,
            language: args.language.clone(),
            safesearch: args.safesearch.clone(),
            livecrawl: args.livecrawl.clone(),
            livecrawl_formats: args.livecrawl_formats.clone(),
            crawl_timeout,
            include_domains,
            exclude_domains,
            boost_domains,
        })
    }
}

fn validate_crawl_timeout(timeout: u16) -> Result<u8, ToolError> {
    if (1..=60).contains(&timeout) {
        u8::try_from(timeout).map_err(|e| ToolError::Config(format!("invalid crawl_timeout: {e}")))
    } else {
        Err(ToolError::Config(
            "web_search crawl_timeout must be between 1 and 60".to_owned(),
        ))
    }
}

fn clean_base_url(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// You.com's `GET /v1/search` response envelope. Result items deserialize
/// straight into [`WebSearchResult`]; cosmetic fields (`thumbnail_url`,
/// `original_thumbnail_url`, `favicon_url`) are not modeled and therefore
/// dropped.
#[derive(Debug, Deserialize)]
struct YouSearchResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    results: YouSearchResults,
    #[serde(default, deserialize_with = "null_as_default")]
    metadata: YouSearchMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct YouSearchResults {
    #[serde(default, deserialize_with = "null_as_default")]
    web: Vec<WebSearchResult>,
    #[serde(default, deserialize_with = "null_as_default")]
    news: Vec<WebSearchResult>,
}

#[derive(Debug, Default, Deserialize)]
struct YouSearchMetadata {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    search_uuid: Option<String>,
    #[serde(default)]
    latency: Option<f64>,
}

impl YouSearchResponse {
    fn into_provider_response(self, query: &str) -> WebSearchProviderResponse {
        WebSearchProviderResponse {
            web: self.results.web,
            news: self.results.news,
            metadata: WebSearchProviderMetadata {
                provider: WebSearchProviderKind::You,
                query: self.metadata.query.unwrap_or_else(|| query.to_owned()),
                search_uuid: self.metadata.search_uuid,
                latency: self.metadata.latency,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::{WebSearchFilters, WebSearchUserLocation};

    fn args(json: &str) -> WebSearchArguments {
        WebSearchArguments::from_json(json).unwrap()
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let provider = YouSearchProvider::with_api_key(
            Arc::new(reqwest::Client::new()),
            "super-secret-key".to_owned(),
            "https://api.example",
        );
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("super-secret-key"));
        assert!(rendered.contains("ApiKey(<redacted>)"));
        assert_eq!(format!("{:?}", ApiKey("k".to_owned())), "ApiKey(<redacted>)");
    }

    #[test]
    fn from_values_treats_blank_credentials_as_unset() {
        let provider = YouSearchProvider::from_values(
            Arc::new(reqwest::Client::new()),
            Some("  ".to_owned()),
            Some(" https://api.example/// ".to_owned()),
        );
        assert!(provider.api_key.is_none());
        assert_eq!(provider.base_url.as_deref(), Some("https://api.example"));

        let provider = YouSearchProvider::with_api_key(Arc::new(reqwest::Client::new()), "k".to_owned(), "");
        assert!(provider.base_url.is_none());
    }

    #[test]
    fn request_renders_every_argument_in_you_order() {
        let args = args(
            r#"{"query":" rust ","count":3,"freshness":"2024-01-01to2024-02-01","country":"us","language":"en",
                "safesearch":"strict","livecrawl":"web","livecrawl_formats":["markdown","html"],"crawl_timeout":5,
                "exclude_domains":["a.example"],"boost_domains":["b.example"]}"#,
        );
        let request = YouSearchRequest::from_args_and_config(" rust ", &args, &WebSearchToolParam::default()).unwrap();
        assert_eq!(
            request.query_params(),
            [
                ("query", "rust"),
                ("count", "3"),
                ("freshness", "2024-01-01to2024-02-01"),
                ("country", "US"),
                ("language", "en"),
                ("safesearch", "strict"),
                ("livecrawl", "web"),
                ("livecrawl_formats", "markdown"),
                ("livecrawl_formats", "html"),
                ("crawl_timeout", "5"),
                ("exclude_domains", "a.example"),
                ("boost_domains", "b.example"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
    }

    #[test]
    fn request_applies_context_size_default_and_tool_config_overrides() {
        let config = WebSearchToolParam {
            search_context_size: Some(WebSearchContextSize::Low),
            filters: Some(WebSearchFilters {
                allowed_domains: Some(vec![" docs.example ".to_owned()]),
                blocked_domains: None,
            }),
            user_location: Some(WebSearchUserLocation {
                country: Some(" de ".to_owned()),
                ..WebSearchUserLocation::default()
            }),
        };
        let args = args(r#"{"query":"rust","country":"us","include_domains":["other.example"]}"#);
        let request = YouSearchRequest::from_args_and_config("rust", &args, &config).unwrap();
        assert_eq!(request.count, Some(WebSearchContextSize::Low.default_count()));
        assert_eq!(request.country.as_deref(), Some("DE"));
        assert_eq!(request.include_domains, Some(vec!["docs.example".to_owned()]));
    }

    #[test]
    fn request_rejects_conflicting_domain_lists_and_out_of_range_values() {
        let params = WebSearchToolParam::default();
        let conflict = args(r#"{"query":"rust","include_domains":["a.example"],"exclude_domains":["b.example"]}"#);
        assert_eq!(
            YouSearchRequest::from_args_and_config("rust", &conflict, &params)
                .unwrap_err()
                .to_string(),
            "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains"
        );
        let count = args(r#"{"query":"rust","count":0}"#);
        assert_eq!(
            YouSearchRequest::from_args_and_config("rust", &count, &params)
                .unwrap_err()
                .to_string(),
            "invalid tool config: web_search count must be between 1 and 100"
        );
        let timeout = args(r#"{"query":"rust","crawl_timeout":61}"#);
        assert_eq!(
            YouSearchRequest::from_args_and_config("rust", &timeout, &params)
                .unwrap_err()
                .to_string(),
            "invalid tool config: web_search crawl_timeout must be between 1 and 60"
        );
    }

    #[test]
    fn response_maps_documented_fields_and_tolerates_nulls() {
        let response: YouSearchResponse = serde_json::from_str(
            r#"{
                "results": {
                    "web": [{
                        "url": "https://example.com/rust",
                        "title": "Rust",
                        "description": "desc",
                        "snippets": null,
                        "thumbnail_url": "https://img.example/t.png",
                        "favicon_url": "https://img.example/f.ico",
                        "page_age": "2024-01-01T00:00:00",
                        "contents": {"markdown": "Rust intro", "highlights": ["a", "b"]}
                    }],
                    "news": null
                },
                "metadata": {"search_uuid": "s1", "latency": 0.5}
            }"#,
        )
        .unwrap();
        let mapped = response.into_provider_response("rust");
        assert_eq!(mapped.news, Vec::new());
        assert_eq!(mapped.metadata.provider, WebSearchProviderKind::You);
        assert_eq!(mapped.metadata.query, "rust");
        assert_eq!(mapped.metadata.search_uuid.as_deref(), Some("s1"));
        assert_eq!(mapped.metadata.latency, Some(0.5));
        let result = &mapped.web[0];
        assert_eq!(result.url, "https://example.com/rust");
        assert_eq!(result.page_age.as_deref(), Some("2024-01-01T00:00:00"));
        assert!(result.snippets.is_empty());
        let contents = result.contents.as_ref().unwrap();
        assert_eq!(contents.markdown.as_deref(), Some("Rust intro"));
        assert_eq!(contents.html, None);
        assert_eq!(contents.highlights, ["a", "b"]);
        assert_eq!(
            serde_json::to_string(result).unwrap(),
            r#"{"url":"https://example.com/rust","title":"Rust","description":"desc","page_age":"2024-01-01T00:00:00","contents":{"markdown":"Rust intro","highlights":["a","b"]}}"#
        );
    }

    #[test]
    fn response_tolerates_missing_envelope_sections() {
        let response: YouSearchResponse = serde_json::from_str("{}").unwrap();
        let mapped = response.into_provider_response("rust");
        assert!(mapped.web.is_empty());
        assert!(mapped.news.is_empty());
        assert_eq!(mapped.metadata.query, "rust");
        assert_eq!(mapped.metadata.search_uuid, None);
        assert_eq!(mapped.metadata.latency, None);

        let response: YouSearchResponse = serde_json::from_str(r#"{"results": null, "metadata": null}"#).unwrap();
        assert!(response.into_provider_response("rust").web.is_empty());
    }

    #[test]
    fn missing_or_null_metadata_serializes_with_submitted_query() {
        for json in [
            "{}",
            r#"{"metadata":null}"#,
            r#"{"metadata":{}}"#,
            r#"{"metadata":{"query":null}}"#,
        ] {
            let response: YouSearchResponse = serde_json::from_str(json).unwrap();
            let mapped = response.into_provider_response("rust");
            assert_eq!(
                serde_json::to_string(&mapped.metadata).unwrap(),
                r#"{"query":"rust"}"#,
                "{json}"
            );
        }
    }
}
