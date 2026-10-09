//! Declaration parameters for the gateway-executed `web_fetch` tool.
//!
//! A native Messages `web_fetch_20250910` declaration is read by the tool
//! layer's Messages mapping (`tool::registry_tools`) and carried into the
//! request-scoped registry as the `WebFetch` kind of its internal
//! `ToolDeclaration`. The tool has no Responses wire form, so this shape is
//! never deserialized from a request body; it holds only what the handler
//! needs for every call of one request.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::domain::DomainFilters;

/// Per-request settings of one `web_fetch` declaration.
///
/// `max_uses` is not here: the Messages loop enforces it as a request-wide
/// budget before a call is dispatched, the same way it does for `web_search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebFetchToolParam {
    /// `allowed_domains` / `blocked_domains`, matched on the URL host only.
    pub filters: Option<DomainFilters>,
    /// Approximate ceiling on the text returned to the model, in tokens.
    pub max_content_tokens: Option<NonZeroU32>,
}

impl WebFetchToolParam {
    /// Reads the shared parameters of a Messages `web_fetch_*` declaration.
    /// `field` looks up one declaration field by name, so the adapter and the
    /// registry seam read a declaration through the same parser.
    ///
    /// Domain lists must be arrays of strings and `max_content_tokens` a
    /// positive integer that fits 32 bits; whether the entries are usable
    /// hosts is the handler's `validate`.
    ///
    /// # Errors
    ///
    /// Returns the field name and the shape it must have.
    pub fn from_declaration<'a>(field: impl Fn(&str) -> Option<&'a Value>) -> Result<Self, String> {
        let domains = |name: &str| -> Result<Option<Vec<String>>, String> {
            let Some(value) = field(name) else {
                return Ok(None);
            };
            value
                .as_array()
                .ok_or_else(|| format!("{name} must be an array of strings"))?
                .iter()
                .map(|entry| {
                    entry
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("{name} must be an array of strings"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        };
        let allowed_domains = domains("allowed_domains")?;
        let blocked_domains = domains("blocked_domains")?;
        let filters = (allowed_domains.is_some() || blocked_domains.is_some()).then_some(DomainFilters {
            allowed_domains,
            blocked_domains,
        });
        let max_content_tokens = match field("max_content_tokens") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|tokens| u32::try_from(tokens).ok())
                    .and_then(NonZeroU32::new)
                    .ok_or_else(|| "max_content_tokens must be a positive integer that fits 32 bits".to_owned())?,
            ),
        };
        Ok(Self {
            filters,
            max_content_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn declaration_fields_are_read_into_typed_parameters() {
        let declaration = json!({"allowed_domains": ["example.com"], "max_content_tokens": 5000, "max_uses": 2});
        let param = WebFetchToolParam::from_declaration(|field| declaration.get(field)).unwrap();
        assert_eq!(
            param
                .filters
                .as_ref()
                .and_then(|filters| filters.allowed_domains.clone()),
            Some(vec!["example.com".to_owned()])
        );
        assert_eq!(param.max_content_tokens.map(NonZeroU32::get), Some(5000));

        let bare = WebFetchToolParam::from_declaration(|_| None).unwrap();
        assert!(bare.filters.is_none());
        assert!(bare.max_content_tokens.is_none());
    }

    #[test]
    fn declaration_shapes_are_checked() {
        for (declaration, expected) in [
            (
                json!({"allowed_domains": "example.com"}),
                "allowed_domains must be an array of strings",
            ),
            (
                json!({"blocked_domains": [1]}),
                "blocked_domains must be an array of strings",
            ),
            (
                json!({"max_content_tokens": 0}),
                "max_content_tokens must be a positive integer",
            ),
            (
                json!({"max_content_tokens": "5"}),
                "max_content_tokens must be a positive integer",
            ),
            (
                json!({"max_content_tokens": 5_000_000_000_u64}),
                "max_content_tokens must be a positive integer",
            ),
        ] {
            let error = WebFetchToolParam::from_declaration(|field| declaration.get(field)).unwrap_err();
            assert!(error.starts_with(expected), "{declaration}: {error}");
        }
    }
}
