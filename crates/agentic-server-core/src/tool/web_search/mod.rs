//! Gateway-owned `web_search` tool.
//!
//! `mod.rs` owns the OpenAI-facing adapter: the [`WebSearchHandler`], the
//! mapping to public `web_search_call` output items. [`provider`] defines the
//! private provider contract, normalized result types, and the response helpers every provider shares. [`args`]
//! parses the model's arguments; provider modules ([`you`], [`brave`], [`tavily`]) shape requests and map responses.

pub(crate) mod args;
pub(crate) mod brave;
mod provider;
pub(crate) mod tavily;
pub(crate) mod you;

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;

use self::args::{MAX_WEB_SEARCH_QUERIES, WebSearchArguments};
use self::brave::BraveSearchProvider;
use self::provider::{
    ApiKey, WebSearchProvider, WebSearchProviderMetadata, WebSearchProviderResponse, WebSearchResult, clean_base_url,
    null_as_default, read_response_limited,
};
use self::tavily::TavilySearchProvider;
use self::you::{YOU_API_BASE_URL, YOU_API_KEY, YouSearchProvider};
use super::handler::MAX_GATEWAY_TOOL_OUTPUT_BYTES;
use super::handler::{GatewayExecutor, GatewayToolEventPlan, ToolError, ToolHandler, ToolOutput};
use super::ownership::GatewayBinding;
use super::registry::{ToolEntry, ToolType};
use crate::config::{DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, WebSearchProviderConfig, WebSearchProviderKind};
use crate::types::io::output::{FunctionToolCall, WebSearchCall, WebSearchCallStatus, WebSearchSource};
use crate::types::io::{FunctionTool, OutputItem};
use crate::types::tools::WebSearchToolParam;

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("serialized JSON size overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) type WebSearchExecutor =
    dyn GatewayExecutor<ToolParams = WebSearchToolParam, ExecutionParams = WebSearchToolParam>;

pub(crate) fn insert_web_search_entry(
    entries: &mut HashMap<String, ToolEntry>,
    params: &WebSearchToolParam,
    executor: Arc<WebSearchExecutor>,
) {
    entries.insert(
        "web_search".to_owned(),
        ToolEntry::gateway(
            ToolType::WebSearch,
            None,
            Some(GatewayBinding::new(executor, params.clone())),
        ),
    );
}

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
                "queries": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "maxItems": MAX_WEB_SEARCH_QUERIES,
                    "description": "Multiple independent search queries to run in parallel, instead of a single query."
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
            "anyOf": [
                {"required": ["query"]},
                {"required": ["queries"]}
            ]
        })),
        strict: Some(false),
    }
}

#[must_use]
pub(crate) fn output_item(
    call: &FunctionToolCall,
    output: &ToolOutput,
    status: WebSearchCallStatus,
) -> Option<OutputItem> {
    let parsed_output = serde_json::from_str::<Value>(&output.output).ok();
    let queries = parsed_output
        .as_ref()
        .and_then(queries_from_value)
        .or_else(|| queries_from_arguments(&call.arguments))
        .unwrap_or_else(|| vec![String::new()]);
    let sources = parsed_output.as_ref().map(sources_from_output).unwrap_or_default();
    WebSearchCall::try_new(call_output_id(call), status, queries, sources)
        .map(OutputItem::WebSearchCall)
        .ok()
}

#[must_use]
pub(crate) fn started_output_item(call: &FunctionToolCall) -> Option<OutputItem> {
    WebSearchCall::try_new(
        call_output_id(call),
        WebSearchCallStatus::InProgress,
        queries_from_arguments(&call.arguments).unwrap_or_else(|| vec![String::new()]),
        Vec::new(),
    )
    .map(OutputItem::WebSearchCall)
    .ok()
}

#[derive(Debug, Clone)]
pub struct WebSearchHandler {
    provider: Option<Arc<dyn WebSearchProvider>>,
    max_concurrent_queries: NonZeroUsize,
    query_permits: Arc<Semaphore>,
}

impl WebSearchHandler {
    #[must_use]
    pub fn from_env(client: Arc<reqwest::Client>) -> Self {
        Self::from_values(
            client,
            std::env::var(YOU_API_KEY).ok(),
            std::env::var(YOU_API_BASE_URL).ok(),
            DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
        )
    }

    /// Builds the You.com-backed handler.
    ///
    /// `max_concurrent_queries` is the gateway-wide ceiling; the provider's own
    /// [`WebSearchProvider::max_concurrent_requests`] ceiling caps it again.
    #[must_use]
    pub fn from_values(
        client: Arc<reqwest::Client>,
        api_key: Option<String>,
        base_url: Option<String>,
        max_concurrent_queries: NonZeroUsize,
    ) -> Self {
        let provider = Arc::new(YouSearchProvider::from_values(client, api_key, base_url));
        let effective = effective_query_concurrency(provider.as_ref(), max_concurrent_queries);
        Self::with_provider_and_query_concurrency(provider, effective)
    }

    /// Builds the handler for the provider selected in `config`.
    ///
    /// The query ceiling is the smallest of the gateway-wide limit, the
    /// operator's `max_concurrent_queries` override, and the provider's own
    /// [`WebSearchProvider::max_concurrent_requests`] ceiling.
    #[must_use]
    pub fn from_config(
        client: Arc<reqwest::Client>,
        config: &WebSearchProviderConfig,
        max_concurrent_gateway_calls: NonZeroUsize,
    ) -> Self {
        let requested = config
            .max_concurrent_queries
            .map_or(max_concurrent_gateway_calls, |ceiling| {
                ceiling.min(max_concurrent_gateway_calls)
            });
        let provider: Arc<dyn WebSearchProvider> = match config.provider {
            WebSearchProviderKind::You => Arc::new(YouSearchProvider::from_values(
                client,
                config.api_key.clone(),
                config.base_url.clone(),
            )),
            WebSearchProviderKind::Brave => Arc::new(BraveSearchProvider::from_values(
                client,
                config.api_key.clone(),
                config.base_url.clone(),
                config
                    .max_concurrent_queries
                    .or(WebSearchProviderKind::Brave.default_max_concurrent_queries())
                    .unwrap_or(max_concurrent_gateway_calls),
            )),
            WebSearchProviderKind::Tavily => Arc::new(TavilySearchProvider::from_values(
                client,
                config.api_key.clone(),
                config.base_url.clone(),
                config
                    .max_concurrent_queries
                    .or(WebSearchProviderKind::Tavily.default_max_concurrent_queries())
                    .unwrap_or(max_concurrent_gateway_calls),
            )),
        };
        let effective = effective_query_concurrency(provider.as_ref(), requested);
        Self::with_provider_and_query_concurrency(provider, effective)
    }

    #[must_use]
    pub fn with_api_key(client: Arc<reqwest::Client>, api_key: String, base_url: &str) -> Self {
        Self::with_provider_and_query_concurrency(
            Arc::new(YouSearchProvider::with_api_key(client, api_key, base_url)),
            DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
        )
    }

    /// Builds a handler usable only for shaping placeholder/error output
    /// (`ToolHandler::normalize`, `GatewayExecutor::started_output`/`public_output`)
    /// when no real provider is configured — `execute()` always fails.
    #[must_use]
    pub fn spec_only() -> Self {
        Self {
            provider: None,
            max_concurrent_queries: DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
            query_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS.get())),
        }
    }

    #[cfg(test)]
    fn with_provider(provider: Arc<dyn WebSearchProvider>) -> Self {
        Self::with_provider_and_query_concurrency(provider, DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS)
    }

    fn with_provider_and_query_concurrency(
        provider: Arc<dyn WebSearchProvider>,
        max_concurrent_queries: NonZeroUsize,
    ) -> Self {
        Self {
            provider: Some(provider),
            max_concurrent_queries,
            query_permits: Arc::new(Semaphore::new(max_concurrent_queries.get())),
        }
    }

    async fn execute_search(
        &self,
        call_id: &str,
        arguments: &str,
        params: &WebSearchToolParam,
    ) -> Result<ToolOutput, ToolError> {
        let provider = self
            .provider
            .as_ref()
            .ok_or_else(|| ToolError::Config("web_search spec-only handler cannot execute tools".to_owned()))?;
        let args = WebSearchArguments::from_json(arguments)?;
        let queries = args.queries();
        let args_ref = &args;
        let mut responses = Box::pin(
            futures::stream::iter(queries.iter().cloned())
                .map(|query| {
                    let provider = Arc::clone(provider);
                    let query_permits = Arc::clone(&self.query_permits);
                    async move {
                        let _permit = query_permits.acquire_owned().await.map_err(|error| {
                            ToolError::Execution(format!("web_search query scheduler closed: {error}"))
                        })?;
                        provider.search(&query, args_ref, params).await
                    }
                })
                .buffered(self.max_concurrent_queries.get()),
        );

        let mut results = WebSearchResultSections::default();
        let mut metadata = Vec::with_capacity(queries.len());
        let mut accumulated_bytes = 0usize;
        while let Some(response) = responses.try_next().await? {
            let mut counter = CountingWriter::default();
            serde_json::to_writer(&mut counter, &response.web)
                .and_then(|()| serde_json::to_writer(&mut counter, &response.news))
                .and_then(|()| serde_json::to_writer(&mut counter, &response.metadata))
                .map_err(|error| ToolError::Execution(format!("failed to size web_search output: {error}")))?;
            accumulated_bytes = accumulated_bytes.saturating_add(counter.bytes);
            if accumulated_bytes > MAX_GATEWAY_TOOL_OUTPUT_BYTES {
                return Err(ToolError::Execution(format!(
                    "web_search output exceeded {MAX_GATEWAY_TOOL_OUTPUT_BYTES} bytes"
                )));
            }
            results.web.extend(response.web);
            results.news.extend(response.news);
            metadata.push(response.metadata);
        }
        let output = serde_json::to_string(&WebSearchToolOutput {
            query: &queries[0],
            queries,
            results,
            metadata,
        })
        .map_err(|e| ToolError::Execution(format!("failed to serialize web_search output: {e}")))?;
        if output.len() > MAX_GATEWAY_TOOL_OUTPUT_BYTES {
            return Err(ToolError::Execution(format!(
                "web_search output exceeded {MAX_GATEWAY_TOOL_OUTPUT_BYTES} bytes"
            )));
        }

        Ok(ToolOutput {
            call_id: call_id.to_owned(),
            output,
        })
    }
}

/// Caps the requested query concurrency at the provider's own ceiling.
fn effective_query_concurrency(provider: &dyn WebSearchProvider, requested: NonZeroUsize) -> NonZeroUsize {
    provider
        .max_concurrent_requests()
        .map_or(requested, |ceiling| requested.min(ceiling))
}

#[derive(Debug, Default, Serialize)]
struct WebSearchResultSections {
    web: Vec<WebSearchResult>,
    news: Vec<WebSearchResult>,
}

/// Model-facing `web_search` tool output; field order is the wire contract.
#[derive(Debug, Serialize)]
struct WebSearchToolOutput<'a> {
    query: &'a str,
    queries: &'a [String],
    results: WebSearchResultSections,
    metadata: Vec<WebSearchProviderMetadata>,
}

impl ToolHandler for WebSearchHandler {
    type ToolParams = WebSearchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::WebSearch
    }

    fn validate(&self, _params: &WebSearchToolParam) -> Result<(), ToolError> {
        Ok(())
    }

    fn normalize(&self, _params: &WebSearchToolParam) -> Vec<FunctionTool> {
        vec![web_search_function_tool()]
    }
}

impl GatewayExecutor for WebSearchHandler {
    type ExecutionParams = WebSearchToolParam;

    fn execute(
        &self,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        params: &WebSearchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let call_id = call_id.to_owned();
        let tool_name = tool_name.to_owned();
        let arguments = arguments.to_owned();
        let params = params.clone();
        Box::pin(async move {
            if tool_name != "web_search" {
                return Err(ToolError::Config(format!(
                    "web_search handler cannot execute tool '{tool_name}'"
                )));
            }
            self.execute_search(&call_id, &arguments, &params).await
        })
    }

    fn supports_parallel_execution(&self) -> bool {
        true
    }

    fn plan_gateway_events(&self, call: &FunctionToolCall, _params: &WebSearchToolParam) -> GatewayToolEventPlan {
        GatewayToolEventPlan::new(started_output_item(call))
    }

    fn public_output(
        &self,
        call: &FunctionToolCall,
        output: &ToolOutput,
        status: WebSearchCallStatus,
        _params: &WebSearchToolParam,
    ) -> Option<OutputItem> {
        output_item(call, output, status)
    }
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

fn queries_from_value(value: &Value) -> Option<Vec<String>> {
    let queries: Vec<String> = value
        .get("queries")?
        .as_array()?
        .iter()
        .filter_map(|item| clean_json_str(Some(item)))
        .collect();
    (!queries.is_empty()).then_some(queries)
}

fn queries_from_arguments(arguments: &str) -> Option<Vec<String>> {
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    queries_from_value(&args).or_else(|| clean_json_str(args.get("query")).map(|query| vec![query]))
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::body::Body;
    use bytes::Bytes;

    use super::*;

    fn metadata(provider_label: &str) -> WebSearchProviderMetadata {
        WebSearchProviderMetadata {
            provider: WebSearchProviderKind::You,
            query: provider_label.to_owned(),
            search_uuid: None,
            latency: None,
        }
    }

    #[derive(Debug)]
    struct MockSearchProvider;

    impl WebSearchProvider for MockSearchProvider {
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _args: &'a WebSearchArguments,
            _config: &'a WebSearchToolParam,
        ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
            Box::pin(async move {
                Ok(WebSearchProviderResponse {
                    web: vec![WebSearchResult {
                        url: "https://example.com/potato".to_owned(),
                        title: Some("Potato".to_owned()),
                        ..WebSearchResult::default()
                    }],
                    news: Vec::new(),
                    metadata: metadata("mock"),
                })
            })
        }
    }

    #[derive(Debug)]
    struct LargeSearchProvider;

    impl WebSearchProvider for LargeSearchProvider {
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _args: &'a WebSearchArguments,
            _config: &'a WebSearchToolParam,
        ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
            Box::pin(async move {
                Ok(WebSearchProviderResponse {
                    web: vec![WebSearchResult {
                        snippets: vec!["x".repeat(600 * 1024)],
                        ..WebSearchResult::default()
                    }],
                    news: Vec::new(),
                    metadata: metadata("large"),
                })
            })
        }
    }

    #[derive(Debug, Default)]
    struct ConcurrencyTrackingProvider {
        active: AtomicUsize,
        max_active: AtomicUsize,
        ceiling: Option<NonZeroUsize>,
    }

    impl WebSearchProvider for ConcurrencyTrackingProvider {
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _args: &'a WebSearchArguments,
            _config: &'a WebSearchToolParam,
        ) -> Pin<Box<dyn Future<Output = Result<WebSearchProviderResponse, ToolError>> + Send + 'a>> {
            Box::pin(async move {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(WebSearchProviderResponse {
                    web: Vec::new(),
                    news: Vec::new(),
                    metadata: metadata("tracking"),
                })
            })
        }

        fn max_concurrent_requests(&self) -> Option<NonZeroUsize> {
            self.ceiling
        }
    }

    #[test]
    fn web_search_schema_caps_batched_queries() {
        let parameters = web_search_function_tool().parameters.expect("web_search parameters");
        assert_eq!(parameters["properties"]["queries"]["maxItems"], MAX_WEB_SEARCH_QUERIES);
    }

    #[tokio::test]
    async fn you_search_response_body_is_bounded_while_reading() {
        let chunk = Bytes::from(vec![b'x'; MAX_GATEWAY_TOOL_OUTPUT_BYTES / 2 + 1]);
        let app = axum::Router::new().route(
            "/v1/search",
            axum::routing::get(move || {
                let chunks = [Ok::<_, std::convert::Infallible>(chunk.clone()), Ok(chunk.clone())];
                async move { Body::from_stream(futures::stream::iter(chunks)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind search response limit server");
        let address = listener.local_addr().expect("search response limit server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/v1/search"))
            .send()
            .await
            .expect("fetch oversized search response");

        let error = read_response_limited(response, WebSearchProviderKind::You)
            .await
            .expect_err("oversized search response must fail");
        assert!(error.to_string().contains("search response exceeded"));
        server.abort();
    }

    #[tokio::test]
    async fn web_search_handler_delegates_to_provider() {
        let handler = WebSearchHandler::with_provider(Arc::new(MockSearchProvider));
        let output = handler
            .execute(
                "call_search",
                "web_search",
                r#"{"query":" potato "}"#,
                &WebSearchToolParam::default(),
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(output.call_id, "call_search");
        assert_eq!(body["query"], "potato");
        assert_eq!(body["queries"], serde_json::json!(["potato"]));
        assert_eq!(body["metadata"][0]["query"], "mock");
        assert_eq!(body["metadata"][0].get("provider"), None);
        assert_eq!(body["results"]["web"][0]["url"], "https://example.com/potato");
    }

    #[tokio::test]
    async fn web_search_handler_fans_out_multiple_queries() {
        let handler = WebSearchHandler::with_provider(Arc::new(MockSearchProvider));
        let output = handler
            .execute(
                "call_search",
                "web_search",
                r#"{"queries":["potato","tomato"]}"#,
                &WebSearchToolParam::default(),
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(body["query"], "potato");
        assert_eq!(body["queries"], serde_json::json!(["potato", "tomato"]));
        assert_eq!(body["results"]["web"].as_array().unwrap().len(), 2);
        assert_eq!(body["metadata"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn web_search_handler_bounds_aggregate_query_results() {
        let handler = WebSearchHandler::with_provider(Arc::new(LargeSearchProvider));
        let error = handler
            .execute(
                "call_search",
                "web_search",
                r#"{"queries":["potato","tomato"]}"#,
                &WebSearchToolParam::default(),
            )
            .await
            .expect_err("aggregate query results must be bounded");

        assert!(error.to_string().contains("web_search output exceeded"));
    }

    #[tokio::test]
    async fn web_search_handler_rejects_oversized_query_batches() {
        let handler = WebSearchHandler::with_provider(Arc::new(MockSearchProvider));
        let error = handler
            .execute(
                "call_search",
                "web_search",
                r#"{"queries":["one","two","three","four","five","six"]}"#,
                &WebSearchToolParam::default(),
            )
            .await
            .expect_err("oversized query batch must fail");

        assert_eq!(
            error.to_string(),
            format!("invalid tool config: web_search accepts at most {MAX_WEB_SEARCH_QUERIES} queries per call")
        );
    }

    #[tokio::test]
    async fn web_search_handler_shares_query_concurrency_across_calls() {
        let provider = Arc::new(ConcurrencyTrackingProvider::default());
        let handler = WebSearchHandler::with_provider_and_query_concurrency(
            provider.clone(),
            NonZeroUsize::new(2).expect("nonzero test limit"),
        );
        let params = WebSearchToolParam::default();
        let arguments = r#"{"queries":["one","two","three","four","five"]}"#;

        let (first, second) = tokio::join!(
            handler.execute("call_one", "web_search", arguments, &params),
            handler.execute("call_two", "web_search", arguments, &params),
        );

        first.expect("first batched call");
        second.expect("second batched call");
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn web_search_output_serializes_in_wire_order() {
        let output = WebSearchToolOutput {
            query: "potato",
            queries: &["potato".to_owned()],
            results: WebSearchResultSections {
                web: vec![WebSearchResult {
                    url: "https://example.com/potato".to_owned(),
                    title: Some("Potato".to_owned()),
                    description: Some("A tuber".to_owned()),
                    snippets: vec!["Starchy.".to_owned()],
                    page_age: None,
                    contents: None,
                }],
                news: vec![WebSearchResult::default()],
            },
            metadata: vec![WebSearchProviderMetadata {
                provider: WebSearchProviderKind::You,
                query: "potato".to_owned(),
                search_uuid: Some("s1".to_owned()),
                latency: Some(0.12),
            }],
        };
        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            concat!(
                r#"{"query":"potato","queries":["potato"],"#,
                r#""results":{"web":[{"url":"https://example.com/potato","title":"Potato","#,
                r#""description":"A tuber","snippets":["Starchy."]}],"news":[{}]},"#,
                r#""metadata":[{"query":"potato","search_uuid":"s1","latency":0.12}]}"#
            )
        );
    }

    #[test]
    fn from_values_inherits_gateway_concurrency() {
        let handler = WebSearchHandler::from_values(
            Arc::new(reqwest::Client::new()),
            None,
            None,
            NonZeroUsize::new(7).expect("nonzero test limit"),
        );
        assert_eq!(handler.max_concurrent_queries.get(), 7);
        assert_eq!(handler.query_permits.available_permits(), 7);
        assert!(format!("{handler:?}").contains("YouSearchProvider"));
    }

    #[test]
    fn from_config_selects_provider_and_resolves_query_ceiling() {
        let gateway_limit = NonZeroUsize::new(5).expect("nonzero test limit");
        let client = Arc::new(reqwest::Client::new());

        // You.com inherits the gateway limit unless the operator lowers it.
        let you = WebSearchProviderConfig::new(Some("k".to_owned()), Some("https://you.example".to_owned()));
        let handler = WebSearchHandler::from_config(Arc::clone(&client), &you, gateway_limit);
        assert!(format!("{handler:?}").contains("YouSearchProvider"));
        assert_eq!(handler.max_concurrent_queries.get(), 5);
        let lowered = you.clone().with_max_concurrent_queries(NonZeroUsize::new(2));
        let handler = WebSearchHandler::from_config(Arc::clone(&client), &lowered, gateway_limit);
        assert_eq!(handler.max_concurrent_queries.get(), 2);
        let raised = you.with_max_concurrent_queries(NonZeroUsize::new(9));
        let handler = WebSearchHandler::from_config(Arc::clone(&client), &raised, gateway_limit);
        assert_eq!(
            handler.max_concurrent_queries.get(),
            5,
            "gateway limit still caps the override"
        );

        // Brave defaults to serial queries; the override can raise the ceiling.
        let brave =
            WebSearchProviderConfig::new(Some("k".to_owned()), None).with_provider(WebSearchProviderKind::Brave);
        let handler = WebSearchHandler::from_config(Arc::clone(&client), &brave, gateway_limit);
        assert!(format!("{handler:?}").contains("BraveSearchProvider"));
        assert!(!format!("{handler:?}").contains("k\""));
        assert_eq!(handler.max_concurrent_queries.get(), 1);
        assert_eq!(handler.query_permits.available_permits(), 1);
        let raised = brave.with_max_concurrent_queries(NonZeroUsize::new(3));
        let handler = WebSearchHandler::from_config(client, &raised, gateway_limit);
        assert_eq!(handler.max_concurrent_queries.get(), 3);
    }

    #[test]
    fn from_values_does_not_leak_api_key_in_debug_output() {
        let handler = WebSearchHandler::from_values(
            Arc::new(reqwest::Client::new()),
            Some("super-secret-key".to_owned()),
            Some("https://api.example".to_owned()),
            DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
        );
        let rendered = format!("{handler:?}");
        assert!(!rendered.contains("super-secret-key"));
        assert!(rendered.contains("ApiKey(<redacted>)"));
    }

    #[test]
    fn effective_query_concurrency_respects_provider_ceiling() {
        let requested = NonZeroUsize::new(4).expect("nonzero test limit");
        let unlimited = ConcurrencyTrackingProvider::default();
        assert_eq!(effective_query_concurrency(&unlimited, requested), requested);

        let capped = ConcurrencyTrackingProvider {
            ceiling: NonZeroUsize::new(1),
            ..ConcurrencyTrackingProvider::default()
        };
        assert_eq!(effective_query_concurrency(&capped, requested).get(), 1);

        let roomy = ConcurrencyTrackingProvider {
            ceiling: NonZeroUsize::new(8),
            ..ConcurrencyTrackingProvider::default()
        };
        assert_eq!(effective_query_concurrency(&roomy, requested), requested);
    }

    #[tokio::test]
    async fn provider_concurrency_ceiling_caps_requested_limit() {
        let provider = Arc::new(ConcurrencyTrackingProvider {
            ceiling: NonZeroUsize::new(1),
            ..ConcurrencyTrackingProvider::default()
        });
        let requested = NonZeroUsize::new(4).expect("nonzero test limit");
        let effective = effective_query_concurrency(provider.as_ref(), requested);
        let handler = WebSearchHandler::with_provider_and_query_concurrency(provider.clone(), effective);
        let params = WebSearchToolParam::default();
        let arguments = r#"{"queries":["one","two","three"]}"#;

        let (first, second) = tokio::join!(
            handler.execute("call_one", "web_search", arguments, &params),
            handler.execute("call_two", "web_search", arguments, &params),
        );

        first.expect("first batched call");
        second.expect("second batched call");
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 1);
    }
}
