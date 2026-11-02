//! Anthropic server tool blocks for the native web tools the gateway runs.
//!
//! A server tool call reaches the client as a `server_tool_use` block and a
//! result block (`web_search_tool_result` or `web_fetch_tool_result`) paired by
//! `tool_use_id`. These types model the fields the gateway generates and reads
//! back on later turns; the call's `input` stays the model's arbitrary JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A `server_tool_use` block: a tool call the server ran for the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerToolUseBlock {
    #[serde(rename = "type")]
    kind: ServerToolUseKind,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

impl ServerToolUseBlock {
    #[must_use]
    pub fn new(id: String, name: String, input: Value) -> Self {
        Self {
            kind: ServerToolUseKind::ServerToolUse,
            id,
            name,
            input,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ServerToolUseKind {
    ServerToolUse,
}

/// The result block answering a `server_tool_use` with the same `tool_use_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToolResultBlock {
    WebSearchToolResult {
        tool_use_id: String,
        content: WebSearchToolResultContent,
    },
    WebFetchToolResult {
        tool_use_id: String,
        content: WebFetchToolResultContent,
    },
}

impl ServerToolResultBlock {
    /// The id of the `server_tool_use` block this result answers.
    #[must_use]
    pub fn tool_use_id(&self) -> &str {
        match self {
            Self::WebSearchToolResult { tool_use_id, .. } | Self::WebFetchToolResult { tool_use_id, .. } => tool_use_id,
        }
    }

    /// The tool name a `server_tool_use` must carry for this result to answer it.
    #[must_use]
    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::WebSearchToolResult { .. } => "web_search",
            Self::WebFetchToolResult { .. } => "web_fetch",
        }
    }

    /// The block's `type`.
    #[must_use]
    pub fn block_type(&self) -> &'static str {
        match self {
            Self::WebSearchToolResult { .. } => "web_search_tool_result",
            Self::WebFetchToolResult { .. } => "web_fetch_tool_result",
        }
    }
}

/// `web_search_tool_result.content`: the results, or a single error object. An
/// empty list is a search that matched nothing, not an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebSearchToolResultContent {
    Results(Vec<WebSearchResultBlock>),
    Error(WebSearchToolResultError),
}

/// One search result. The page content travels only in `encrypted_content`,
/// which the client must send back unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResultBlock {
    #[serde(rename = "type")]
    kind: WebSearchResultKind,
    pub url: String,
    #[serde(default)]
    pub title: String,
    pub encrypted_content: String,
    #[serde(default)]
    pub page_age: Option<String>,
}

impl WebSearchResultBlock {
    #[must_use]
    pub fn new(url: String, title: String, encrypted_content: String, page_age: Option<String>) -> Self {
        Self {
            kind: WebSearchResultKind::WebSearchResult,
            url,
            title,
            encrypted_content,
            page_age,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchResultKind {
    WebSearchResult,
}

/// A search that did not run or did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchToolResultError {
    #[serde(rename = "type")]
    kind: WebSearchToolResultErrorKind,
    pub error_code: WebSearchErrorCode,
}

impl WebSearchToolResultError {
    #[must_use]
    pub fn new(error_code: WebSearchErrorCode) -> Self {
        Self {
            kind: WebSearchToolResultErrorKind::WebSearchToolResultError,
            error_code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchToolResultErrorKind {
    WebSearchToolResultError,
}

/// The documented `web_search_tool_result_error` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchErrorCode {
    TooManyRequests,
    InvalidToolInput,
    MaxUsesExceeded,
    QueryTooLong,
    RequestTooLarge,
    Unavailable,
}

/// `web_fetch_tool_result.content`: the fetched document, or an error. Beyond
/// Anthropic's fields, an error may carry the `message` the gateway gave the
/// model, so a later turn can show the model the same text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebFetchToolResultContent {
    WebFetchResult(WebFetchResult),
    WebFetchToolResultError {
        error_code: WebFetchErrorCode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// A fetched page as plain text, in Anthropic's document shape. Beyond
/// Anthropic's fields, it may carry the page's original `content_type` and
/// whether its text was `truncated`, as the gateway reported them to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchResult {
    pub url: String,
    pub content: WebFetchDocument,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// The document of a [`WebFetchResult`]. The gateway returns extracted text,
/// so the source is always `text/plain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchDocument {
    #[serde(rename = "type")]
    kind: WebFetchDocumentKind,
    pub source: PlainTextSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl WebFetchDocument {
    #[must_use]
    pub fn new(data: String, title: Option<String>) -> Self {
        Self {
            kind: WebFetchDocumentKind::Document,
            source: PlainTextSource {
                kind: PlainTextSourceKind::Text,
                media_type: PlainTextMediaType::TextPlain,
                data,
            },
            title,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebFetchDocumentKind {
    Document,
}

/// A `text` document source whose media type is `text/plain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlainTextSource {
    #[serde(rename = "type")]
    kind: PlainTextSourceKind,
    media_type: PlainTextMediaType,
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlainTextSourceKind {
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PlainTextMediaType {
    #[serde(rename = "text/plain")]
    TextPlain,
}

/// The documented `web_fetch_tool_result_error` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebFetchErrorCode {
    InvalidToolInput,
    UrlTooLong,
    UrlNotAllowed,
    UrlNotInPriorContext,
    UrlNotAccessible,
    TooManyRequests,
    UnsupportedContentType,
    MaxUsesExceeded,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(wire: &Value) -> T {
        let parsed: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            &serde_json::to_value(&parsed).unwrap(),
            wire,
            "serializes back unchanged"
        );
        parsed
    }

    #[test]
    fn the_documented_web_search_shapes_round_trip() {
        let call: ServerToolUseBlock = round_trip(&json!({
            "type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search",
            "input": {"query": "claude shannon birth date"}
        }));
        assert_eq!(call.input["query"], "claude shannon birth date");

        let results: ServerToolResultBlock = round_trip(&json!({
            "type": "web_search_tool_result", "tool_use_id": "srvtoolu_01",
            "content": [{"type": "web_search_result", "url": "https://en.wikipedia.org/wiki/Claude_Shannon",
                         "title": "Claude Shannon - Wikipedia", "encrypted_content": "EqgfCioIARgB",
                         "page_age": "April 30, 2025"}]
        }));
        assert_eq!(results.tool_use_id(), "srvtoolu_01");
        assert_eq!(results.tool_name(), "web_search");
        assert_eq!(results.block_type(), "web_search_tool_result");

        let empty: ServerToolResultBlock = round_trip(&json!({
            "type": "web_search_tool_result", "tool_use_id": "srvtoolu_02", "content": []
        }));
        assert!(matches!(
            empty,
            ServerToolResultBlock::WebSearchToolResult { content: WebSearchToolResultContent::Results(ref list), .. }
                if list.is_empty()
        ));

        let error: ServerToolResultBlock = round_trip(&json!({
            "type": "web_search_tool_result", "tool_use_id": "srvtoolu_a93jad",
            "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}
        }));
        assert_eq!(
            error,
            ServerToolResultBlock::WebSearchToolResult {
                tool_use_id: "srvtoolu_a93jad".to_owned(),
                content: WebSearchToolResultContent::Error(WebSearchToolResultError::new(
                    WebSearchErrorCode::MaxUsesExceeded
                )),
            }
        );
        // A result without a page age serializes it as null, as Anthropic does.
        let block = WebSearchResultBlock::new("https://a".to_owned(), String::new(), "e".to_owned(), None);
        assert_eq!(serde_json::to_value(block).unwrap()["page_age"], Value::Null);
    }

    #[test]
    fn the_documented_web_fetch_shapes_round_trip() {
        let document: ServerToolResultBlock = round_trip(&json!({
            "type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_01H",
            "content": {"type": "web_fetch_result", "url": "https://example.com/article",
                        "content": {"type": "document",
                                    "source": {"type": "text", "media_type": "text/plain",
                                               "data": "Full text content of the article..."},
                                    "title": "Article"},
                        "retrieved_at": "2025-08-25T10:30:00Z"}
        }));
        assert_eq!(document.tool_name(), "web_fetch");
        assert_eq!(document.block_type(), "web_fetch_tool_result");
        // The documented example without a title or retrieval time parses too.
        let _: ServerToolResultBlock = round_trip(&json!({
            "type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_01H",
            "content": {"type": "web_fetch_result", "url": "https://example.com/article",
                        "content": {"type": "document",
                                    "source": {"type": "text", "media_type": "text/plain", "data": "x"}}}
        }));
        let error: ServerToolResultBlock = round_trip(&json!({
            "type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_01H",
            "content": {"type": "web_fetch_tool_result_error", "error_code": "url_not_accessible"}
        }));
        assert!(matches!(
            error,
            ServerToolResultBlock::WebFetchToolResult {
                content: WebFetchToolResultContent::WebFetchToolResultError {
                    error_code: WebFetchErrorCode::UrlNotAccessible,
                    message: None,
                },
                ..
            }
        ));
        // The gateway's additions round-trip with the documented fields.
        let _: ServerToolResultBlock = round_trip(&json!({
            "type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_gw_t",
            "content": {"type": "web_fetch_result", "url": "https://a",
                        "content": {"type": "document",
                                    "source": {"type": "text", "media_type": "text/plain", "data": "x"}},
                        "retrieved_at": "2026-10-30T12:00:00Z", "content_type": "text/html", "truncated": true}
        }));
        let _: ServerToolResultBlock = round_trip(&json!({
            "type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_gw_t",
            "content": {"type": "web_fetch_tool_result_error", "error_code": "url_not_accessible",
                        "message": "HTTP 404 Not Found"}
        }));
    }

    #[test]
    fn shapes_the_gateway_does_not_generate_are_refused() {
        for wire in [
            // A PDF document is base64, not the gateway's plain text.
            json!({"type": "web_fetch_tool_result", "tool_use_id": "t",
                   "content": {"type": "web_fetch_result", "url": "https://a",
                               "content": {"type": "document",
                                           "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBE"}}}}),
            json!({"type": "web_search_tool_result", "tool_use_id": "t",
                   "content": {"type": "web_search_tool_result_error", "error_code": "not_a_code"}}),
            json!({"type": "web_search_tool_result", "tool_use_id": "t",
                   "content": [{"type": "web_search_result", "url": "https://a", "title": "t"}]}),
            json!({"type": "code_execution_tool_result", "tool_use_id": "t", "content": {}}),
        ] {
            assert!(
                serde_json::from_value::<ServerToolResultBlock>(wire.clone()).is_err(),
                "{wire}"
            );
        }
        assert!(
            serde_json::from_value::<ServerToolUseBlock>(json!({"type": "tool_use", "id": "t", "name": "n"})).is_err()
        );
    }
}
