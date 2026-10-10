//! Gateway-executed `web_fetch` tool for the Anthropic Messages API (#408).
//!
//! A native `web_fetch_20250910` declaration is rewritten into an ordinary
//! function tool for the upstream model, and the resulting `web_fetch` call is
//! executed here instead of reaching a client that expects the server to have
//! run it. This module owns the model-facing policy: argument parsing, URL
//! admission ([`policy`]), domain filtering through the shared
//! [`domain_policy`](super::domain_policy), text extraction ([`extract`]), the
//! content limit, the ceiling on fetches in flight, and the output shape
//! ([`output`]). Retrieval sits behind [`backend::WebFetchBackend`]; the
//! built-in [`http::HttpFetchBackend`] is the default, and replacing it is a
//! change to this module alone, not to the Messages loop.
//!
//! A call produces one [`output::WebFetchOutcome`]: a rendered page, or a
//! refusal with one of the documented `web_fetch_tool_result_error` codes.
//! Both are serialized once, at the model-facing boundary; a refusal is `Ok`
//! output with a failure status, which the Messages loop reports as
//! `is_error`. `Err` is reserved for gateway faults.

pub(crate) mod backend;
mod extract;
mod http;
mod output;
pub(crate) mod policy;

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use self::backend::{FetchFailure, FetchedDocument, WebFetchBackend};
use self::http::HttpFetchBackend;
use self::output::{Refusal, WebFetchOutcome, render_document};
pub(crate) use self::output::{WebFetchErrorCode, failure_output};
use super::domain_policy::{DomainFilter, validate_domain_filters};
use super::handler::{GatewayExecutor, ToolError, ToolHandler, ToolOutput};
use super::ownership::GatewayBinding;
use super::registry::{ToolEntry, ToolType};
use crate::config::{DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, WebFetchConfig};
use crate::types::io::FunctionTool;
use crate::types::tools::WebFetchToolParam;

/// The registry key and model-visible name of the tool.
pub(crate) const WEB_FETCH_TOOL_NAME: &str = "web_fetch";

pub(crate) type WebFetchExecutor =
    dyn GatewayExecutor<ToolParams = WebFetchToolParam, ExecutionParams = WebFetchToolParam>;

pub(crate) fn insert_web_fetch_entry(
    entries: &mut HashMap<String, ToolEntry>,
    params: &WebFetchToolParam,
    executor: Arc<WebFetchExecutor>,
) {
    entries.insert(
        WEB_FETCH_TOOL_NAME.to_owned(),
        ToolEntry::gateway(
            ToolType::WebFetch,
            None,
            Some(GatewayBinding::new(executor, params.clone())),
        ),
    );
}

/// The function tool the upstream model sees in place of the native declaration.
#[must_use]
pub(crate) fn web_fetch_function_tool() -> FunctionTool {
    FunctionTool {
        type_: "function".to_owned(),
        name: WEB_FETCH_TOOL_NAME.to_owned(),
        description: Some(
            "Fetch the full text of one web page. Only a URL that already appears in the conversation can be \
             fetched: one the user wrote, one returned by a tool, or one from an earlier search or fetch result."
                .to_owned(),
        ),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The absolute http or https URL of the page to fetch."
                }
            },
            "required": ["url"]
        })),
        strict: Some(false),
    }
}

/// Validated arguments of one `web_fetch` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebFetchArguments {
    pub(crate) url: String,
}

impl WebFetchArguments {
    /// Parse the model's arguments: a JSON object with a non-empty string `url`.
    pub(crate) fn from_json(arguments: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(arguments).map_err(|error| format!("arguments must be valid JSON: {error}"))?;
        let url = value
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| "arguments must contain a non-empty string url".to_owned())?;
        Ok(Self { url: url.to_owned() })
    }
}

/// Executes `web_fetch` calls against one backend.
#[derive(Debug, Clone)]
pub struct WebFetchHandler {
    backend: Arc<dyn WebFetchBackend>,
    /// Fetches in flight at once across the gateway. The Messages loop starts
    /// a round's admitted calls together, so this is what bounds the open
    /// connections one turn can hold; a permit is held until extraction ends.
    permits: Arc<Semaphore>,
}

/// The backend of a specification-only handler: every fetch is unavailable.
#[derive(Debug)]
struct NoBackend;

impl WebFetchBackend for NoBackend {
    fn fetch<'a>(
        &'a self,
        _url: &'a Url,
        _filter: &'a DomainFilter,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>> {
        Box::pin(async { Err(FetchFailure::NoBackend) })
    }
}

impl WebFetchHandler {
    /// Builds the handler on the built-in HTTP backend, with at most
    /// `max_concurrent_fetches` fetches in flight at once.
    #[must_use]
    pub fn from_config(config: &WebFetchConfig, max_concurrent_fetches: NonZeroUsize) -> Self {
        Self::with_backend(Arc::new(HttpFetchBackend::new(config.clone())), max_concurrent_fetches)
    }

    /// A handler without a backend, for validating and normalizing
    /// declarations where no fetch will run; `execute` answers `unavailable`.
    #[must_use]
    pub fn spec_only() -> Self {
        Self::with_backend(Arc::new(NoBackend), DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS)
    }

    pub(crate) fn with_backend(backend: Arc<dyn WebFetchBackend>, max_concurrent_fetches: NonZeroUsize) -> Self {
        Self {
            backend,
            permits: Arc::new(Semaphore::new(max_concurrent_fetches.get())),
        }
    }

    async fn execute_fetch(
        &self,
        call_id: &str,
        arguments: &str,
        params: &WebFetchToolParam,
    ) -> Result<ToolOutput, ToolError> {
        let outcome = match self.fetch(arguments, params).await {
            Ok((document, permit)) => {
                // Extracting a body up to the download ceiling is CPU work: it
                // runs off the async workers and holds the fetch permit until
                // it finishes, so admission stays bounded even after the
                // awaiting call has timed out.
                let params = params.clone();
                let rendered = tokio::task::spawn_blocking(move || {
                    let rendered = render_document(document, &params);
                    drop(permit);
                    rendered
                })
                .await
                .map_err(|error| ToolError::Execution(format!("web_fetch extraction task failed: {error}")))?;
                WebFetchOutcome::Document(rendered)
            }
            Err(refusal) => WebFetchOutcome::Refused(refusal),
        };
        outcome.into_tool_output(call_id)
    }

    /// Admit the call and retrieve the page. The returned permit is the call's
    /// slot among the fetches in flight; the caller holds it through extraction.
    async fn fetch(
        &self,
        arguments: &str,
        params: &WebFetchToolParam,
    ) -> Result<(FetchedDocument, OwnedSemaphorePermit), Refusal> {
        let args = WebFetchArguments::from_json(arguments)
            .map_err(|reason| Refusal::new(WebFetchErrorCode::InvalidToolInput, reason))?;
        let url = policy::validate_url(&args.url)
            .map_err(|rejection| Refusal::new(rejection.code(), rejection.to_string()))?;
        let filter = DomainFilter::from_filters(params.filters.as_ref());
        if !filter.allows(url.as_str()) {
            return Err(Refusal::new(
                WebFetchErrorCode::UrlNotAllowed,
                format!("{} is outside the allowed domains", url.host_str().unwrap_or_default()),
            ));
        }
        let permit = Arc::clone(&self.permits).acquire_owned().await.map_err(|error| {
            Refusal::new(
                WebFetchErrorCode::Unavailable,
                format!("fetch scheduler closed: {error}"),
            )
        })?;
        let document = self
            .backend
            .fetch(&url, &filter)
            .await
            .map_err(|failure| Refusal::from_failure(&failure))?;
        Ok((document, permit))
    }
}

impl ToolHandler for WebFetchHandler {
    type ToolParams = WebFetchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::WebFetch
    }

    /// The shared declaration parameters: the domain lists must name hosts
    /// and are mutually exclusive, the rule both web tools share.
    /// Anthropic-specific settings (tool version, citations, cache) are the
    /// Messages adapter's to judge.
    fn validate(&self, params: &WebFetchToolParam) -> Result<(), ToolError> {
        validate_domain_filters(WEB_FETCH_TOOL_NAME, params.filters.as_ref())
    }

    fn normalize(&self, _params: &WebFetchToolParam) -> Vec<FunctionTool> {
        vec![web_fetch_function_tool()]
    }
}

impl GatewayExecutor for WebFetchHandler {
    type ExecutionParams = WebFetchToolParam;

    fn execute(
        &self,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        params: &WebFetchToolParam,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let call_id = call_id.to_owned();
        let tool_name = tool_name.to_owned();
        let arguments = arguments.to_owned();
        let params = params.clone();
        Box::pin(async move {
            if tool_name != WEB_FETCH_TOOL_NAME {
                return Err(ToolError::Config(format!(
                    "web_fetch handler cannot execute tool '{tool_name}'"
                )));
            }
            self.execute_fetch(&call_id, &arguments, &params).await
        })
    }

    fn supports_parallel_execution(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use reqwest::StatusCode;

    use super::policy::UrlRejection;
    use super::*;
    use crate::tool::handler::MAX_GATEWAY_TOOL_OUTPUT_BYTES;
    use crate::types::tools::DomainFilters;

    fn handler(backend: Arc<dyn WebFetchBackend>) -> WebFetchHandler {
        WebFetchHandler::with_backend(backend, DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS)
    }

    /// A backend that answers every URL with one fixed document, or with the
    /// failure its constructor builds.
    #[derive(Debug)]
    struct FixedBackend {
        media_type: &'static str,
        body: String,
        truncated: bool,
        failure: Option<fn() -> FetchFailure>,
    }

    impl FixedBackend {
        fn html(body: &str) -> Arc<Self> {
            Arc::new(Self {
                media_type: "text/html",
                body: body.to_owned(),
                truncated: false,
                failure: None,
            })
        }

        fn failing(failure: fn() -> FetchFailure) -> Arc<Self> {
            Arc::new(Self {
                media_type: "text/plain",
                body: String::new(),
                truncated: false,
                failure: Some(failure),
            })
        }
    }

    impl WebFetchBackend for FixedBackend {
        fn fetch<'a>(
            &'a self,
            url: &'a Url,
            _filter: &'a DomainFilter,
        ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>> {
            Box::pin(async move {
                if let Some(failure) = self.failure {
                    return Err(failure());
                }
                Ok(FetchedDocument {
                    url: url.clone(),
                    media_type: self.media_type.to_owned(),
                    body: self.body.clone(),
                    truncated: self.truncated,
                })
            })
        }
    }

    async fn run(handler: &WebFetchHandler, arguments: &str, params: &WebFetchToolParam) -> Value {
        let output = handler
            .execute("call_fetch", WEB_FETCH_TOOL_NAME, arguments, params)
            .await
            .expect("documented failures are outputs, not errors");
        assert_eq!(output.call_id, "call_fetch");
        serde_json::from_str(&output.output).expect("JSON output")
    }

    fn error_code(output: &Value) -> &str {
        assert_eq!(output["type"], "web_fetch_tool_result_error", "{output}");
        output["error_code"].as_str().expect("error_code")
    }

    fn allowlist(domains: &[&str]) -> WebFetchToolParam {
        WebFetchToolParam {
            filters: Some(DomainFilters {
                allowed_domains: Some(domains.iter().map(|domain| (*domain).to_owned()).collect()),
                blocked_domains: None,
            }),
            max_content_tokens: None,
        }
    }

    #[test]
    fn upstream_schema_requires_a_single_url() {
        let tool = web_fetch_function_tool();
        assert_eq!(tool.name, "web_fetch");
        let parameters = tool.parameters.expect("parameters");
        assert_eq!(parameters["required"], serde_json::json!(["url"]));
        assert_eq!(parameters["properties"]["url"]["type"], "string");
    }

    #[test]
    fn arguments_parse_once_for_the_budget_and_the_url() {
        let args = WebFetchArguments::from_json(r#"{"url":" https://example.com/a "}"#).unwrap();
        assert_eq!(args.url, "https://example.com/a");
        for rejected in [r#"{"url":""}"#, r#"{"url":5}"#, "{}", "{not json", r#"{"urls":["x"]}"#] {
            assert!(WebFetchArguments::from_json(rejected).is_err(), "{rejected}");
        }
    }

    #[tokio::test]
    async fn html_is_extracted_into_a_titled_text_result() {
        let handler = handler(FixedBackend::html(
            "<html><head><title>Doc &amp; Co</title></head><body><h1>Hi</h1><p>Body <b>text</b>.</p></body></html>",
        ));
        let output = run(
            &handler,
            r#"{"url":"https://example.com/doc"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(output["type"], "web_fetch_result");
        assert_eq!(output["url"], "https://example.com/doc");
        assert_eq!(output["title"], "Doc & Co");
        assert_eq!(output["content_type"], "text/html");
        assert_eq!(output["content"], "Hi\nBody text.");
        assert_eq!(output["truncated"], false);
        assert!(output["retrieved_at"].as_str().unwrap().ends_with('Z'));
    }

    #[tokio::test]
    async fn plain_text_passes_through_and_the_content_limit_truncates() {
        let backend = Arc::new(FixedBackend {
            media_type: "text/plain",
            body: "é".repeat(100),
            truncated: false,
            failure: None,
        });
        let handler = handler(backend);
        let params = WebFetchToolParam {
            filters: None,
            max_content_tokens: NonZeroU32::new(10),
        };
        let output = run(&handler, r#"{"url":"https://example.com/t"}"#, &params).await;
        assert!(output.get("title").is_none(), "plain text has no title");
        // 10 tokens * 4 bytes = 40 bytes; `é` is 2 bytes, so 20 characters survive.
        assert_eq!(output["content"], "é".repeat(20));
        assert_eq!(output["truncated"], true);

        let untouched = run(
            &handler,
            r#"{"url":"https://example.com/t"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(untouched["content"], "é".repeat(100));
        assert_eq!(untouched["truncated"], false);
    }

    #[tokio::test]
    async fn a_backend_truncation_is_reported() {
        let backend = Arc::new(FixedBackend {
            media_type: "text/plain",
            body: "partial".to_owned(),
            truncated: true,
            failure: None,
        });
        let handler = handler(backend);
        let output = run(
            &handler,
            r#"{"url":"https://example.com/big"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(output["truncated"], true);
        assert_eq!(output["content"], "partial");
    }

    #[tokio::test]
    async fn output_stays_under_the_gateway_cap_after_json_escaping() {
        let backend = Arc::new(FixedBackend {
            media_type: "text/plain",
            body: "\"\n".repeat(MAX_GATEWAY_TOOL_OUTPUT_BYTES),
            truncated: false,
            failure: None,
        });
        let handler = handler(backend);
        let output = handler
            .execute(
                "c",
                WEB_FETCH_TOOL_NAME,
                r#"{"url":"https://example.com/escaped"}"#,
                &WebFetchToolParam::default(),
            )
            .await
            .unwrap();
        assert!(
            output.output.len() <= MAX_GATEWAY_TOOL_OUTPUT_BYTES,
            "{}",
            output.output.len()
        );
        let value: Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(value["truncated"], true);
        assert!(!value["content"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_arguments_and_urls_report_documented_codes_without_fetching() {
        let handler = handler(FixedBackend::failing(|| FetchFailure::NoBackend));
        let params = WebFetchToolParam::default();
        for (arguments, expected) in [
            (r#"{"url":""}"#, "invalid_tool_input"),
            ("{not json", "invalid_tool_input"),
            (r#"{"url":"example.com/page"}"#, "invalid_tool_input"),
            (r#"{"url":"ftp://example.com/f"}"#, "invalid_tool_input"),
            (r#"{"url":"https://user:pw@example.com/"}"#, "url_not_allowed"),
        ] {
            let output = run(&handler, arguments, &params).await;
            assert_eq!(error_code(&output), expected, "{arguments}");
        }
        let long = format!(r#"{{"url":"https://example.com/{}"}}"#, "x".repeat(300));
        let output = run(&handler, &long, &params).await;
        assert_eq!(error_code(&output), "url_too_long");
        assert_eq!(output["message"], "url exceeds 250 characters");
    }

    #[tokio::test]
    async fn domain_filters_apply_to_the_requested_url() {
        let handler = handler(FixedBackend::html("<p>ok</p>"));
        let allowed = run(
            &handler,
            r#"{"url":"https://docs.example.com/a"}"#,
            &allowlist(&["example.com"]),
        )
        .await;
        assert_eq!(allowed["content"], "ok");
        let refused = run(
            &handler,
            r#"{"url":"https://example.org/a"}"#,
            &allowlist(&["example.com"]),
        )
        .await;
        assert_eq!(error_code(&refused), "url_not_allowed");

        let blocklist = WebFetchToolParam {
            filters: Some(DomainFilters {
                allowed_domains: None,
                blocked_domains: Some(vec!["example.com".to_owned()]),
            }),
            max_content_tokens: None,
        };
        let refused = run(&handler, r#"{"url":"https://api.example.com/a"}"#, &blocklist).await;
        assert_eq!(error_code(&refused), "url_not_allowed");
        let allowed = run(&handler, r#"{"url":"https://example.org/a"}"#, &blocklist).await;
        assert_eq!(allowed["content"], "ok");
    }

    #[tokio::test]
    async fn an_allowlist_that_names_no_host_admits_nothing() {
        let handler = handler(FixedBackend::html("<p>ok</p>"));
        let refused = run(&handler, r#"{"url":"https://example.com/a"}"#, &allowlist(&["."])).await;
        assert_eq!(error_code(&refused), "url_not_allowed", "{refused}");
    }

    #[tokio::test]
    async fn backend_failures_map_onto_documented_codes() {
        let params = WebFetchToolParam::default();
        for (failure, expected) in [
            (
                (|| FetchFailure::NotPublic {
                    host: "10.0.0.1".to_owned(),
                }) as fn() -> FetchFailure,
                "url_not_allowed",
            ),
            (
                || FetchFailure::RedirectRefused(Box::new(FetchFailure::Rejected(UrlRejection::Credentials))),
                "url_not_allowed",
            ),
            (|| FetchFailure::Status(StatusCode::NOT_FOUND), "url_not_accessible"),
            (
                || FetchFailure::Dns {
                    host: "x.invalid".to_owned(),
                    source: std::io::Error::other("no such host"),
                },
                "url_not_accessible",
            ),
            (|| FetchFailure::TimedOut, "url_not_accessible"),
            (|| FetchFailure::TooManyRedirects(5), "url_not_accessible"),
            (|| FetchFailure::TooManyRequests, "too_many_requests"),
            (
                || FetchFailure::UnsupportedContentType("application/pdf".to_owned()),
                "unsupported_content_type",
            ),
            (|| FetchFailure::NoBackend, "unavailable"),
        ] {
            let handler = handler(FixedBackend::failing(failure));
            let output = run(&handler, r#"{"url":"https://example.com/x"}"#, &params).await;
            assert_eq!(error_code(&output), expected, "{output}");
            assert!(output["message"].as_str().is_some_and(|message| !message.is_empty()));
        }
    }

    #[tokio::test]
    async fn only_the_web_fetch_name_is_executed() {
        let handler = handler(FixedBackend::html("<p>ok</p>"));
        let error = handler
            .execute(
                "c",
                "web_search",
                r#"{"url":"https://example.com/"}"#,
                &WebFetchToolParam::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Config(message) if message.contains("cannot execute tool 'web_search'")));
    }

    #[tokio::test]
    async fn a_spec_only_handler_validates_and_normalizes_but_fetches_nothing() {
        let handler = WebFetchHandler::spec_only();
        assert!(handler.validate(&WebFetchToolParam::default()).is_ok());
        assert!(handler.validate(&allowlist(&["Example.COM.", "93.184.216.34"])).is_ok());
        let error = handler.validate(&allowlist(&["."])).unwrap_err();
        assert!(
            matches!(&error, ToolError::Config(message) if message == "web_fetch allowed_domains entry \".\" is not a host name"),
            "{error}"
        );
        let error = handler.validate(&allowlist(&["https://example.com"])).unwrap_err();
        assert!(error.to_string().contains("without a scheme or path"), "{error}");
        let both = WebFetchToolParam {
            filters: Some(DomainFilters {
                allowed_domains: Some(vec!["a.com".to_owned()]),
                blocked_domains: Some(vec!["b.com".to_owned()]),
            }),
            max_content_tokens: None,
        };
        let error = handler.validate(&both).unwrap_err();
        assert!(error.to_string().contains("cannot be used together"), "{error}");
        assert_eq!(handler.normalize(&WebFetchToolParam::default())[0].name, "web_fetch");

        let output = run(
            &handler,
            r#"{"url":"https://example.com/"}"#,
            &WebFetchToolParam::default(),
        )
        .await;
        assert_eq!(error_code(&output), "unavailable");
    }

    /// A backend that records how many fetches are in flight at once.
    #[derive(Debug, Default)]
    struct CountingBackend {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    impl WebFetchBackend for CountingBackend {
        fn fetch<'a>(
            &'a self,
            url: &'a Url,
            _filter: &'a DomainFilter,
        ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>> {
            Box::pin(async move {
                let active = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(FetchedDocument {
                    url: url.clone(),
                    media_type: "text/plain".to_owned(),
                    body: "ok".to_owned(),
                    truncated: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn fetches_in_flight_are_capped_by_the_concurrency_ceiling() {
        let backend = Arc::new(CountingBackend::default());
        let handler = WebFetchHandler::with_backend(
            Arc::clone(&backend) as Arc<dyn WebFetchBackend>,
            NonZeroUsize::new(2).expect("nonzero"),
        );
        let params = WebFetchToolParam::default();
        let ids: Vec<String> = (0..6).map(|index| format!("c{index}")).collect();
        let outputs = futures::future::join_all(
            ids.iter()
                .map(|id| handler.execute(id, WEB_FETCH_TOOL_NAME, r#"{"url":"https://example.com/"}"#, &params)),
        )
        .await;
        assert!(outputs.iter().all(Result::is_ok));
        assert_eq!(
            backend.peak.load(Ordering::SeqCst),
            2,
            "at most two fetches ran at once"
        );
        assert_eq!(backend.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(
            handler.permits.available_permits(),
            2,
            "every permit came back after extraction"
        );
    }
}
