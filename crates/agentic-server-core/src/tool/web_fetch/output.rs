//! The model-facing result of one `web_fetch` call.
//!
//! A call ends in a [`WebFetchOutcome`]: a page rendered to text, or a
//! [`Refusal`] carrying one of the documented `web_fetch_tool_result_error`
//! codes. Both are serialized here, once, into the two JSON shapes the model
//! receives (`web_fetch_result` and `web_fetch_tool_result_error`), and the
//! output carries its status, so the Messages loop marks a refusal `is_error`
//! without reading the output. A backend failure becomes a documented code in
//! one place, [`Refusal::from_failure`], with its source chain kept in the
//! message.

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use url::Url;

use super::backend::{FetchFailure, FetchedDocument};
use super::extract::{self, truncate_to_char_boundary};
use crate::tool::handler::{MAX_GATEWAY_TOOL_OUTPUT_BYTES, ToolError, ToolOutput};
use crate::types::tools::WebFetchToolParam;

/// Bytes of UTF-8 per token assumed when applying `max_content_tokens`. The
/// limit is documented as approximate.
const BYTES_PER_CONTENT_TOKEN: usize = 4;
/// Ceiling on the text returned to the model, leaving room for the JSON
/// envelope under the gateway tool output cap.
const MAX_CONTENT_BYTES: usize = MAX_GATEWAY_TOOL_OUTPUT_BYTES - 16 * 1024;
/// The `type` of a model-facing result that carries a page.
const RESULT_TYPE: &str = "web_fetch_result";
/// The `type` of a model-facing result that reports a documented failure.
const FAILURE_TYPE: &str = "web_fetch_tool_result_error";

/// The documented `web_fetch_tool_result_error` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WebFetchErrorCode {
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

#[derive(Serialize)]
struct FailureOutput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error_code: WebFetchErrorCode,
    message: &'a str,
}

/// The model-facing output for a fetch that produced no document: the
/// documented error shape plus a `message` the model can act on.
#[must_use]
pub(crate) fn failure_output(error_code: WebFetchErrorCode, message: &str) -> String {
    serde_json::to_string(&FailureOutput {
        kind: FAILURE_TYPE,
        error_code,
        message,
    })
    .unwrap_or_else(|_| {
        r#"{"type":"web_fetch_tool_result_error","error_code":"unavailable","message":"internal error"}"#.to_owned()
    })
}

/// Model-facing `web_fetch` output; field order is the wire contract.
#[derive(Serialize)]
struct WebFetchToolOutput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    content_type: &'a str,
    retrieved_at: &'a str,
    truncated: bool,
    content: &'a str,
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
        let output = serde_json::to_string(&WebFetchToolOutput {
            kind: RESULT_TYPE,
            url: document.url.as_str(),
            title: document.title.as_deref(),
            content_type: &document.media_type,
            retrieved_at: &document.retrieved_at,
            truncated: document.truncated || content.len() < document.text.len(),
            content,
        })
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
