//! The model-facing result of one `web_fetch` call.
//!
//! A call ends in a [`WebFetchOutcome`]: a page rendered to text, or a
//! [`Refusal`] carrying one of the documented `web_fetch_tool_result_error`
//! codes. Both are serialized here, once, into the two JSON shapes the model
//! receives (`web_fetch_result` and `web_fetch_tool_result_error`), and the
//! output carries its status, so the Messages loop marks a refusal `is_error`
//! without reading the output. A backend failure becomes a documented code in
//! one place, [`Refusal::from_failure`], with its source chain kept in the
//! message. The Messages adapter reads both shapes back with the same types
//! when it shows a native call to the client.

use std::borrow::Cow;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use super::backend::{FetchFailure, FetchedDocument};
use super::extract::{self, truncate_to_char_boundary};
use crate::tool::handler::{MAX_GATEWAY_TOOL_OUTPUT_BYTES, ToolError, ToolOutput};
pub(crate) use crate::types::messages::WebFetchErrorCode;
use crate::types::tools::WebFetchToolParam;

/// Bytes of UTF-8 per token assumed when applying `max_content_tokens`. The
/// limit is documented as approximate.
const BYTES_PER_CONTENT_TOKEN: usize = 4;
/// Ceiling on the text returned to the model, leaving room for the JSON
/// envelope under the gateway tool output cap.
const MAX_CONTENT_BYTES: usize = MAX_GATEWAY_TOOL_OUTPUT_BYTES - 16 * 1024;
/// The `type` of a model-facing result that carries a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResultKind {
    WebFetchResult,
}

/// The `type` of a model-facing result that reports a documented failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FailureKind {
    WebFetchToolResultError,
}

/// A fetch the handler refused or could not complete: the documented code and
/// a message the model can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    code: WebFetchErrorCode,
    message: String,
}

impl Refusal {
    pub(super) fn new(code: WebFetchErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The one place a backend failure becomes a documented code. The message
    /// keeps the failure's source chain so the model and the logs see why.
    pub(super) fn from_failure(failure: &FetchFailure) -> Self {
        let code = match failure {
            FetchFailure::NotPublic { .. } | FetchFailure::OutsideDomains { .. } | FetchFailure::RedirectRefused(_) => {
                WebFetchErrorCode::UrlNotAllowed
            }
            FetchFailure::Rejected(rejection) => rejection.code(),
            FetchFailure::InvalidRedirect(_)
            | FetchFailure::UnparseableRedirect(_)
            | FetchFailure::RedirectWithoutLocation { .. }
            | FetchFailure::TooManyRedirects(_)
            | FetchFailure::NoHost
            | FetchFailure::Dns { .. }
            | FetchFailure::NoAddress { .. }
            | FetchFailure::TimedOut
            | FetchFailure::Request(_)
            | FetchFailure::Body(_)
            | FetchFailure::Status(_) => WebFetchErrorCode::UrlNotAccessible,
            FetchFailure::TooManyRequests => WebFetchErrorCode::TooManyRequests,
            FetchFailure::UnsupportedContentType(_) => WebFetchErrorCode::UnsupportedContentType,
            FetchFailure::Client(_) | FetchFailure::NoBackend => WebFetchErrorCode::Unavailable,
        };
        Self::new(code, describe(failure))
    }
}

/// An error's message followed by its source chain, without repeating a cause
/// the message already states.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The model-facing output of a fetch that produced no document: the
/// documented error shape plus a `message` the model can act on.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FailureOutput<'a> {
    #[serde(rename = "type")]
    kind: FailureKind,
    pub(crate) error_code: WebFetchErrorCode,
    pub(crate) message: Cow<'a, str>,
}

/// The model-facing output for a fetch that produced no document.
#[must_use]
pub(crate) fn failure_output(error_code: WebFetchErrorCode, message: &str) -> String {
    serde_json::to_string(&FailureOutput {
        kind: FailureKind::WebFetchToolResultError,
        error_code,
        message: Cow::Borrowed(message),
    })
    .unwrap_or_else(|_| {
        r#"{"type":"web_fetch_tool_result_error","error_code":"unavailable","message":"internal error"}"#.to_owned()
    })
}

/// Model-facing `web_fetch` output for a page; field order is the wire
/// contract. The fetcher sets every field; a page the Messages adapter
/// rebuilds from a returned result block may lack the optional ones.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WebFetchToolOutput<'a> {
    #[serde(rename = "type")]
    kind: ResultKind,
    pub(crate) url: Cow<'a, str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retrieved_at: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncated: Option<bool>,
    pub(crate) content: Cow<'a, str>,
}

impl<'a> WebFetchToolOutput<'a> {
    /// A page as the model reads it.
    #[must_use]
    pub(crate) fn page(
        url: &'a str,
        title: Option<&'a str>,
        content_type: Option<&'a str>,
        retrieved_at: Option<&'a str>,
        truncated: Option<bool>,
        content: &'a str,
    ) -> Self {
        Self {
            kind: ResultKind::WebFetchResult,
            url: Cow::Borrowed(url),
            title: title.map(Cow::Borrowed),
            content_type: content_type.map(Cow::Borrowed),
            retrieved_at: retrieved_at.map(Cow::Borrowed),
            truncated,
            content: Cow::Borrowed(content),
        }
    }
}

/// A page after extraction, ready to serialize for the model.
#[derive(Debug)]
pub(super) struct RenderedDocument {
    url: Url,
    title: Option<String>,
    media_type: String,
    retrieved_at: String,
    truncated: bool,
    text: String,
    content_limit: usize,
}

/// What one call produced; serialized once, at the model-facing boundary.
#[derive(Debug)]
pub(super) enum WebFetchOutcome {
    Document(RenderedDocument),
    Refused(Refusal),
}

impl WebFetchOutcome {
    /// The model-facing output, with its status set by the outcome: a page is
    /// the tool's answer, a refusal a failure the loop reports as an error.
    pub(super) fn into_tool_output(self, call_id: &str) -> Result<ToolOutput, ToolError> {
        Ok(match self {
            Self::Document(document) => ToolOutput::success(call_id, serialize_document(&document)?),
            Self::Refused(refusal) => ToolOutput::failure(call_id, failure_output(refusal.code, &refusal.message)),
        })
    }
}

/// Reduce a fetched document to the text the model receives.
pub(super) fn render_document(document: FetchedDocument, params: &WebFetchToolParam) -> RenderedDocument {
    let FetchedDocument {
        url,
        media_type,
        body,
        truncated,
    } = document;
    let (title, text) = if is_html(&media_type) {
        let extracted = extract::html_to_text(&body);
        (extracted.title, extracted.text)
    } else {
        (None, body)
    };
    RenderedDocument {
        url,
        title,
        media_type,
        retrieved_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        truncated,
        text,
        content_limit: content_limit(params),
    }
}

/// Serialize a rendered document under its content limit, keeping the whole
/// output under the gateway tool output cap even after JSON escaping.
fn serialize_document(document: &RenderedDocument) -> Result<String, ToolError> {
    let mut limit = document.content_limit;
    loop {
        let content = truncate_to_char_boundary(&document.text, limit);
        let output = serde_json::to_string(&WebFetchToolOutput::page(
            document.url.as_str(),
            document.title.as_deref(),
            Some(&document.media_type),
            Some(&document.retrieved_at),
            Some(document.truncated || content.len() < document.text.len()),
            content,
        ))
        .map_err(|error| ToolError::Execution(format!("failed to serialize web_fetch output: {error}")))?;
        if output.len() <= MAX_GATEWAY_TOOL_OUTPUT_BYTES || content.is_empty() {
            return Ok(output);
        }
        // JSON escaping grew the envelope past the cap; cut the text further.
        limit = content.len() * 3 / 4;
    }
}

fn is_html(media_type: &str) -> bool {
    matches!(media_type, "text/html" | "application/xhtml+xml")
}

/// The byte budget for the text returned to the model.
fn content_limit(params: &WebFetchToolParam) -> usize {
    params.max_content_tokens.map_or(MAX_CONTENT_BYTES, |tokens| {
        usize::try_from(tokens.get())
            .unwrap_or(usize::MAX)
            .saturating_mul(BYTES_PER_CONTENT_TOKEN)
            .min(MAX_CONTENT_BYTES)
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::super::policy::UrlRejection;
    use super::*;
    use crate::tool::handler::ToolOutputStatus;

    #[test]
    fn failure_output_uses_the_documented_shape() {
        assert_eq!(
            failure_output(WebFetchErrorCode::UrlNotInPriorContext, "not seen"),
            r#"{"type":"web_fetch_tool_result_error","error_code":"url_not_in_prior_context","message":"not seen"}"#
        );
    }

    #[test]
    fn both_outputs_read_back_into_the_types_that_wrote_them() {
        let page = serde_json::to_string(&WebFetchToolOutput::page(
            "https://example.com/doc",
            Some("Doc"),
            Some("text/html"),
            Some("2026-10-30T12:00:00Z"),
            Some(false),
            "body \"text\"",
        ))
        .unwrap();
        assert_eq!(
            page,
            r#"{"type":"web_fetch_result","url":"https://example.com/doc","title":"Doc","content_type":"text/html","retrieved_at":"2026-10-30T12:00:00Z","truncated":false,"content":"body \"text\""}"#
        );
        let read: WebFetchToolOutput<'_> = serde_json::from_str(&page).unwrap();
        assert_eq!(serde_json::to_string(&read).unwrap(), page);

        let failure = failure_output(WebFetchErrorCode::UrlNotAccessible, "HTTP 404");
        let read: FailureOutput<'_> = serde_json::from_str(&failure).unwrap();
        assert_eq!(read.error_code, WebFetchErrorCode::UrlNotAccessible);
        assert_eq!(read.message, "HTTP 404");
        assert!(
            serde_json::from_str::<FailureOutput<'_>>(&page).is_err(),
            "the kinds do not mix"
        );
        assert!(serde_json::from_str::<WebFetchToolOutput<'_>>(&failure).is_err());
    }

    #[test]
    fn a_document_is_a_success_output_and_a_refusal_a_failure_output() {
        let document = render_document(
            FetchedDocument {
                url: Url::parse("https://example.com/doc").unwrap(),
                media_type: "text/plain".to_owned(),
                body: "hello".to_owned(),
                truncated: false,
            },
            &WebFetchToolParam::default(),
        );
        let output = WebFetchOutcome::Document(document).into_tool_output("call_1").unwrap();
        assert_eq!(output.status, ToolOutputStatus::Success);
        assert!(!output.is_failure());
        assert!(
            output.output.starts_with(r#"{"type":"web_fetch_result","#),
            "{}",
            output.output
        );

        let output = WebFetchOutcome::Refused(Refusal::new(WebFetchErrorCode::UrlNotAccessible, "HTTP 404"))
            .into_tool_output("call_2")
            .unwrap();
        assert_eq!(output.status, ToolOutputStatus::Failure);
        assert!(output.is_failure());
    }

    #[test]
    fn refusals_carry_the_source_chain_and_one_code_per_failure() {
        let dns = FetchFailure::Dns {
            host: "x.invalid".to_owned(),
            source: std::io::Error::other("no such host"),
        };
        let refusal = Refusal::from_failure(&dns);
        assert_eq!(refusal.code, WebFetchErrorCode::UrlNotAccessible);
        assert_eq!(refusal.message, "could not resolve x.invalid: no such host");

        let hop = FetchFailure::RedirectRefused(Box::new(FetchFailure::Rejected(UrlRejection::Credentials)));
        let refusal = Refusal::from_failure(&hop);
        assert_eq!(refusal.code, WebFetchErrorCode::UrlNotAllowed);
        assert_eq!(refusal.message, "redirect target refused: url carries credentials");
    }

    #[test]
    fn a_refusal_serializes_as_the_documented_failure_shape() {
        let output = WebFetchOutcome::Refused(Refusal::new(WebFetchErrorCode::TooManyRequests, "slow down"))
            .into_tool_output("call_1")
            .unwrap();
        assert_eq!(output.call_id, "call_1");
        assert!(output.is_failure());
        assert_eq!(
            output.output,
            r#"{"type":"web_fetch_tool_result_error","error_code":"too_many_requests","message":"slow down"}"#
        );
    }

    #[test]
    fn the_content_limit_follows_max_content_tokens_under_the_cap() {
        assert_eq!(content_limit(&WebFetchToolParam::default()), MAX_CONTENT_BYTES);
        let ten = WebFetchToolParam {
            filters: None,
            max_content_tokens: NonZeroU32::new(10),
        };
        assert_eq!(content_limit(&ten), 10 * BYTES_PER_CONTENT_TOKEN);
        let huge = WebFetchToolParam {
            filters: None,
            max_content_tokens: NonZeroU32::new(u32::MAX),
        };
        assert_eq!(content_limit(&huge), MAX_CONTENT_BYTES);
    }
}
