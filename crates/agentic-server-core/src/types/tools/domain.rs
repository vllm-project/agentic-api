//! Domain lists shared by the web tools' declaration parameters.

use serde::{Deserialize, Serialize};

/// `allowed_domains` / `blocked_domains` of a `web_search` or `web_fetch`
/// declaration: the hosts a tool may reach, or must not.
///
/// This is the wire shape of the Responses `web_search` tool's `filters`
/// field; the Messages adapter reads the Anthropic top-level fields into the
/// same type. How the lists are validated and matched is the tool layer's
/// shared domain policy, not a property of either tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DomainFilters {
    pub allowed_domains: Option<Vec<String>>,
    pub blocked_domains: Option<Vec<String>>,
}
