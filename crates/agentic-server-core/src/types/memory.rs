//! Contracts for the opt-in conversation-memory prototype.
//!
//! These are library contracts, not an `OpenAI` Responses wire extension.

use serde::{Deserialize, Serialize};

use super::file_search::{FileSearchError, RankingOptions, SearchFilter};

/// A resolved application identity. Resolve it from verified authentication
/// before considering any request-supplied fallback. Never pass raw credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryOwner(String);

impl MemoryOwner {
    /// Resolve owner scope without letting an untrusted fallback override authentication.
    /// An invalid authenticated identity fails closed instead of using the fallback.
    ///
    /// # Errors
    /// Returns an invalid-request error for blank or oversized identities.
    pub fn resolve(authenticated: Option<&str>, fallback: Option<&str>) -> Result<Option<Self>, FileSearchError> {
        authenticated
            .or(fallback)
            .map(|owner| {
                if owner.trim().is_empty() || owner.len() > 512 || owner.chars().any(char::is_control) {
                    return Err(FileSearchError::InvalidRequest(
                        "memory owner must contain 1 to 512 bytes without control characters".into(),
                    ));
                }
                Ok(Self(owner.to_owned()))
            })
            .transpose()
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Per-call controls for the library prototype; defaults match OGX's read budget.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryOptions {
    pub enabled: bool,
    pub write_enabled: bool,
    pub max_num_results: usize,
    /// Approximate token budget, enforced as a UTF-8 byte ceiling of four times this value.
    pub max_context_tokens: usize,
    pub filters: Option<SearchFilter>,
    pub ranking_options: Option<RankingOptions>,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            write_enabled: true,
            max_num_results: 5,
            max_context_tokens: 1200,
            filters: None,
            ranking_options: None,
        }
    }
}

impl MemoryOptions {
    /// # Errors
    /// Returns an invalid-request error for an unsupported retrieval budget.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if !(1..=50).contains(&self.max_num_results) || !(1..=8192).contains(&self.max_context_tokens) {
            return Err(FileSearchError::InvalidRequest(
                "memory requires 1 to 50 results and 1 to 8192 approximate context tokens".into(),
            ));
        }
        Ok(())
    }
}

/// One immutable memory artifact. Applications supply a model-generated summary
/// and a bounded transcript; identifiers link it back to the source conversation.
#[derive(Clone, Debug)]
pub struct MemoryDocument<'a> {
    pub conversation_id: &'a str,
    pub response_id: &'a str,
    pub summary: &'a str,
    pub transcript: &'a str,
}
