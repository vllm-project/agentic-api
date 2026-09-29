//! Bounded stage attributes and outbound W3C context propagation.

use opentelemetry::propagation::Injector;
use tracing::{Span, field, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::tool::ToolType;
use crate::types::request_response::RequestPayload;

#[derive(Clone, Copy)]
pub(crate) enum CompactionTrigger {
    ContextManagement,
    InputItem,
    Explicit,
}

impl CompactionTrigger {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ContextManagement => "context_management",
            Self::InputItem => "input_item",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum StateSource {
    None,
    PreviousResponse,
    Conversation,
}

impl StateSource {
    pub(crate) fn from_request(request: &RequestPayload) -> Self {
        if request.conversation_id.is_some() {
            Self::Conversation
        } else if request.previous_response_id.is_some() {
            Self::PreviousResponse
        } else {
            Self::None
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PreviousResponse => "previous_response",
            Self::Conversation => "conversation",
        }
    }
}

pub(crate) const fn tool_type(kind: ToolType) -> &'static str {
    match kind {
        ToolType::Function => "function",
        ToolType::ToolSearch => "tool_search",
        ToolType::Custom => "custom",
        ToolType::Shell => "shell",
        ToolType::CodexNamespace => "codex_namespace",
        ToolType::Mcp => "mcp",
        ToolType::WebSearch => "web_search",
        ToolType::FileSearch => "file_search",
        ToolType::CodeInterpreter => "code_interpreter",
    }
}

pub(crate) fn inference_round(round: usize) -> Span {
    info_span!(
        "agentic.inference_round",
        agentic.inference.round = i64::try_from(round).unwrap_or(i64::MAX)
    )
}

pub(crate) fn http_client(url: &str) -> Span {
    let span = info_span!(
        "http.client.request",
        otel.kind = "client",
        http.request.method = "POST",
        server.address = field::Empty,
        server.port = field::Empty,
        http.response.status_code = field::Empty,
        error.r#type = field::Empty,
        otel.status_code = field::Empty,
    );
    if let Ok(url) = url::Url::parse(url) {
        if let Some(host) = url.host_str() {
            span.record("server.address", host);
        }
        if let Some(port) = url.port_or_known_default() {
            span.record("server.port", i64::from(port));
        }
    }
    span
}

struct HeaderInjector<'a>(&'a mut http::HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(key), Ok(value)) = (http::HeaderName::try_from(key), http::HeaderValue::try_from(value)) {
            self.0.insert(key, value);
        }
    }
}

/// Replace caller context with the active gateway span, including when export is disabled.
pub(crate) fn inject_context(headers: &mut http::HeaderMap) {
    headers.remove("traceparent");
    headers.remove("tracestate");
    let context = Span::current().context();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderInjector(headers));
    });
}
