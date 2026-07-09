use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::io::output::{FunctionToolCall, WebSearchCall, WebSearchCallStatus, WebSearchSource};
use crate::types::io::{FunctionTool, OutputItem};
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};
use crate::utils::common::serialize_to_string;

use super::handler::{GatewayExecutor, ToolError, ToolHandler, ToolOutput};
use super::registry::ToolType;

const YOU_API_KEY: &str = "YOU_API_KEY";
const YOU_API_BASE_URL: &str = "YOU_API_BASE_URL";

#[must_use]
pub(crate) fn web_search_function_tool() -> FunctionTool {
    FunctionTool {
        type_: "function".to_owned(),
        name: "web_search".to_owned(),
        description: Some(
            "Search the public web for current information and return structured web and news results.".to_owned(),
        ),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The natural language web search query."
                },
                "count": {
                    "type": "integer",
                    "description": "Maximum results per section, from 1 to 100."
                },
                "freshness": {
                    "type": "string",
                    "description": "Optional recency filter: day, week, month, year, or YYYY-MM-DDtoYYYY-MM-DD."
                },
                "country": {
                    "type": "string",
                    "description": "Optional ISO 3166-1 alpha-2 country code."
                },
                "language": {
                    "type": "string",
                    "description": "Optional BCP 47 language code."
                },
                "include_domains": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional strict allowlist of domains."
                },
                "exclude_domains": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional domain blocklist."
                }
            },
            "required": ["query"]
        })),
        strict: Some(false),
    }
}

#[must_use]
pub(crate) fn output_item(call: &FunctionToolCall, output: &ToolOutput, status: WebSearchCallStatus) -> OutputItem {
    let parsed_output = serde_json::from_str::<Value>(&output.output).ok();
    let query = parsed_output
        .as_ref()
        .and_then(|value| clean_json_str(value.get("query")))
        .or_else(|| query_from_arguments(&call.arguments))
        .unwrap_or_default();
    let sources = parsed_output.as_ref().map(sources_from_output).unwrap_or_default();
    OutputItem::WebSearchCall(WebSearchCall::new(call_output_id(call), status, query, sources))
}

#[must_use]
pub(crate) fn started_output_item(call: &FunctionToolCall) -> OutputItem {
    OutputItem::WebSearchCall(WebSearchCall::new(
        call_output_id(call),
        WebSearchCallStatus::InProgress,
        query_from_arguments(&call.arguments).unwrap_or_default(),
        Vec::new(),
    ))
}

#[derive(Debug, Clone)]
pub struct WebSearchHandler {
    provider: Arc<dyn WebSearchProvider>,
}

impl WebSearchHandler {
    #[must_use]
    pub fn from_env(client: Arc<reqwest::Client>) -> Self {
        Self {
            provider: Arc::new(YouSearchProvider::from_env(client)),
        }
    }

    #[must_use]
    pub fn with_api_key(client: Arc<reqwest::Client>, api_key: String, base_url: &str) -> Self {
        Self {
            provider: Arc::new(YouSearchProvider::with_api_key(client, api_key, base_url)),
        }
    }

    #[cfg(test)]
    fn with_provider(provider: Arc<dyn WebSearchProvider>) -> Self {
        Self { provider }
    }

    async fn execute_search(&self, call_id: &str, arguments: &str, config: &Value) -> Result<ToolOutput, ToolError> {
        let args = WebSearchArguments::from_json(arguments)?;
        let config = serde_json::from_value::<WebSearchToolParam>(config.clone())
            .map_err(|e| ToolError::Config(format!("invalid web_search config: {e}")))?;
        let response = self.provider.search(&args, &config).await?;
        let output = serde_json::to_string(&serde_json::json!({
            "query": response.query,
            "results": response.results,
            "metadata": response.metadata
        }))
        .map_err(|e| ToolError::Execution(format!("failed to serialize web_search output: {e}")))?;

        Ok(ToolOutput {
            call_id: call_id.to_owned(),
            output,
        })
    }
}

trait WebSearchProvider: std::fmt::Debug + Send + Sync {
    fn search<'a>(
        &'a self,
        args: &'a WebSearchArguments,
        config: &'a WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>>;
}

struct WebSearchProviderResponse {
    query: String,
    results: Value,
    metadata: Value,
}

#[derive(Debug, Clone)]
struct YouSearchProvider {
    client: Arc<reqwest::Client>,
    api_key: Option<String>,
    base_url: Option<String>,
}

impl YouSearchProvider {
    fn from_env(client: Arc<reqwest::Client>) -> Self {
        let api_key = std::env::var(YOU_API_KEY)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let base_url = std::env::var(YOU_API_BASE_URL)
            .ok()
            .and_then(|value| clean_base_url(&value));
        Self {
            client,
            api_key,
            base_url,
        }
    }

    fn with_api_key(client: Arc<reqwest::Client>, api_key: String, base_url: &str) -> Self {
        Self {
            client,
            api_key: Some(api_key),
            base_url: clean_base_url(base_url),
        }
    }
}

impl WebSearchProvider for YouSearchProvider {
    fn search<'a>(
        &'a self,
        args: &'a WebSearchArguments,
        config: &'a WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let api_key = self
                .api_key
                .as_deref()
                .ok_or_else(|| ToolError::Config(format!("{YOU_API_KEY} must be set to use the web_search tool")))?;
            let base_url = self.base_url.as_deref().ok_or_else(|| {
                ToolError::Config(format!("{YOU_API_BASE_URL} must be set to use the web_search tool"))
            })?;
            let request = YouSearchRequest::from_args_and_config(args, config)?;
            let url = format!("{base_url}/v1/search");
            let body = serialize_to_string(&request)
                .map_err(|e| ToolError::Execution(format!("failed to serialize web_search request: {e}")))?;

            let resp = self
                .client
                .post(url)
                .header("X-API-Key", api_key)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("You.com search request failed: {e}")))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(ToolError::Execution(format!(
                    "You.com search returned {status}: {body}"
                )));
            }

            let response_text = resp
                .text()
                .await
                .map_err(|e| ToolError::Execution(format!("failed to read You.com search response: {e}")))?;
            let response: Value = serde_json::from_str(&response_text)
                .map_err(|e| ToolError::Execution(format!("You.com search returned invalid JSON: {e}")))?;
            Ok(WebSearchProviderResponse {
                query: request.query,
                results: response
                    .get("results")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"web": [], "news": []})),
                metadata: response.get("metadata").cloned().unwrap_or(Value::Null),
            })
        })
    }
}

impl ToolHandler for WebSearchHandler {
    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }

    fn validate(&self, param: &Value) -> Result<(), ToolError> {
        serde_json::from_value::<WebSearchToolParam>(param.clone())
            .map(|_| ())
            .map_err(|e| ToolError::Config(format!("invalid web_search config: {e}")))
    }

    fn normalize(&self, _param: &Value) -> Vec<FunctionTool> {
        vec![web_search_function_tool()]
    }
}

impl GatewayExecutor for WebSearchHandler {
    fn execute(
        &self,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        config: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let call_id = call_id.to_owned();
        let tool_name = tool_name.to_owned();
        let arguments = arguments.to_owned();
        let config = config.clone();
        Box::pin(async move {
            if tool_name != "web_search" {
                return Err(ToolError::Config(format!(
                    "web_search handler cannot execute tool '{tool_name}'"
                )));
            }
            self.execute_search(&call_id, &arguments, &config).await
        })
    }
}

#[derive(Debug, Deserialize)]
struct WebSearchArguments {
    query: String,
    count: Option<u16>,
    freshness: Option<String>,
    country: Option<String>,
    language: Option<String>,
    safesearch: Option<String>,
    livecrawl: Option<String>,
    livecrawl_formats: Option<Vec<String>>,
    crawl_timeout: Option<u16>,
    include_domains: Option<Vec<String>>,
    exclude_domains: Option<Vec<String>>,
    boost_domains: Option<Vec<String>>,
}

impl WebSearchArguments {
    fn from_json(arguments: &str) -> Result<Self, ToolError> {
        let args = serde_json::from_str::<Self>(arguments)
            .map_err(|e| ToolError::Config(format!("web_search arguments must be valid JSON: {e}")))?;
        if args.query.trim().is_empty() {
            return Err(ToolError::Config("web_search query must not be empty".to_owned()));
        }
        Ok(args)
    }
}

#[derive(Debug, Serialize)]
struct YouSearchRequest {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    safesearch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    livecrawl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    livecrawl_formats: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    crawl_timeout: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    boost_domains: Option<Vec<String>>,
}

impl YouSearchRequest {
    fn from_args_and_config(args: &WebSearchArguments, config: &WebSearchToolParam) -> Result<Self, ToolError> {
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
        let include_domains = config_domains.or_else(|| clean_vec(args.include_domains.as_deref()));
        let exclude_domains = clean_vec(args.exclude_domains.as_deref());
        let boost_domains = clean_vec(args.boost_domains.as_deref());
        if include_domains.is_some() && (exclude_domains.is_some() || boost_domains.is_some()) {
            return Err(ToolError::Config(
                "include_domains cannot be combined with exclude_domains or boost_domains".to_owned(),
            ));
        }
        let country = config
            .user_location
            .as_ref()
            .and_then(|location| clean_string(location.country.as_deref()))
            .or_else(|| clean_string(args.country.as_deref()))
            .map(|value| value.to_ascii_uppercase());

        Ok(Self {
            query: args.query.trim().to_owned(),
            count,
            freshness: clean_string(args.freshness.as_deref()),
            country,
            language: clean_string(args.language.as_deref()),
            safesearch: clean_string(args.safesearch.as_deref()),
            livecrawl: clean_string(args.livecrawl.as_deref()),
            livecrawl_formats: clean_vec(args.livecrawl_formats.as_deref()),
            crawl_timeout,
            include_domains,
            exclude_domains,
            boost_domains,
        })
    }
}

fn validate_count(count: u16) -> Result<u8, ToolError> {
    if (1..=100).contains(&count) {
        Ok(u8::try_from(count).expect("validated web_search count must fit in u8"))
    } else {
        Err(ToolError::Config(
            "web_search count must be between 1 and 100".to_owned(),
        ))
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

fn clean_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn clean_json_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn call_output_id(call: &FunctionToolCall) -> String {
    if let Some(suffix) = call.id.strip_prefix("fc_").filter(|suffix| !suffix.is_empty()) {
        return format!("ws_{suffix}");
    }
    if let Some(suffix) = call.call_id.strip_prefix("call_").filter(|suffix| !suffix.is_empty()) {
        return format!("ws_{suffix}");
    }
    crate::utils::uuid7_str("ws_")
}

fn query_from_arguments(arguments: &str) -> Option<String> {
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    clean_json_str(args.get("query"))
}

fn sources_from_output(output: &Value) -> Vec<WebSearchSource> {
    ["web", "news"]
        .into_iter()
        .filter_map(|section| output.get("results")?.get(section)?.as_array())
        .flat_map(|results| results.iter())
        .filter_map(source_from_result)
        .collect()
}

fn source_from_result(result: &Value) -> Option<WebSearchSource> {
    let url = clean_json_str(result.get("url"))?;
    Some(WebSearchSource {
        url,
        title: clean_json_str(result.get("title")),
    })
}

fn clean_base_url(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn clean_vec(values: Option<&[String]>) -> Option<Vec<String>> {
    let cleaned: Vec<String> = values
        .unwrap_or_default()
        .iter()
        .filter_map(|value| clean_string(Some(value.as_str())))
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct MockSearchProvider;

    impl WebSearchProvider for MockSearchProvider {
        fn search<'a>(
            &'a self,
            args: &'a WebSearchArguments,
            _config: &'a WebSearchToolParam,
        ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
            Box::pin(async move {
                Ok(WebSearchProviderResponse {
                    query: args.query.trim().to_owned(),
                    results: serde_json::json!({
                        "web": [
                            {
                                "url": "https://example.com/potato",
                                "title": "Potato"
                            }
                        ],
                        "news": []
                    }),
                    metadata: serde_json::json!({"provider": "mock"}),
                })
            })
        }
    }

    #[tokio::test]
    async fn web_search_handler_delegates_to_provider() {
        let handler = WebSearchHandler::with_provider(Arc::new(MockSearchProvider));
        let output = handler
            .execute(
                "call_search",
                "web_search",
                r#"{"query":" potato "}"#,
                &serde_json::json!({"type": "web_search_preview"}),
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(output.call_id, "call_search");
        assert_eq!(body["query"], "potato");
        assert_eq!(body["metadata"]["provider"], "mock");
        assert_eq!(body["results"]["web"][0]["url"], "https://example.com/potato");
    }
}
