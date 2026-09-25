//! Redacted wrapper for errors derived from opaque-provider response bytes.

use std::fmt;

use super::ExecutorError;

/// Retains the typed cause for explicit internal inspection without exposing
/// provider-controlled text in ordinary Display, Debug, or API error envelopes.
/// Do not log source chains: the source may contain reflected opaque state.
#[derive(thiserror::Error)]
#[error("invalid opaque Responses upstream response")]
pub struct OpaqueUpstreamError {
    #[source]
    source: Box<ExecutorError>,
}

impl OpaqueUpstreamError {
    /// The typed cause, for bounded internal classification only; never log it.
    pub(crate) fn cause(&self) -> &ExecutorError {
        &self.source
    }

    pub(crate) fn redact(error: ExecutorError) -> ExecutorError {
        match error {
            // These variants have bounded, structured, non-payload diagnostics.
            ExecutorError::ResourceLimitExceeded { .. }
            | ExecutorError::ReasoningReplay(_)
            | ExecutorError::UpstreamModel(_)
            | ExecutorError::LLMTransport { .. }
            // Preserve the engine's existing failed-response tool-search lifecycle.
            | ExecutorError::Tool(crate::tool::ToolError::InvalidUpstreamToolSearch | crate::tool::ToolError::UpstreamWithheldFunctionCall)
            | ExecutorError::OpaqueUpstream(_) => error,
            source => Self {
                source: Box::new(source),
            }
            .into(),
        }
    }
}

impl fmt::Debug for OpaqueUpstreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OpaqueUpstreamError").finish_non_exhaustive()
    }
}
