//! Shared HTTP query handling for item listings.

use agentic_core::types::io::{ItemInclude, ItemLimit, ItemOrder, ItemPage};
use std::collections::HashSet;

use agentic_core::executor::ExecutorError;
use axum::response::Response;
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};

use super::super::common::executor_error_response;

/// Shared query parameters for conversation and response input item listings.
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct ListItemsQuery {
    /// Maximum number of items to return (default: 20, max: 100).
    #[serde(default)]
    #[cfg_attr(feature = "openapi", param(value_type = Option<i64>, minimum = 0, maximum = 100))]
    pub limit: ItemLimit,

    /// Cursor for pagination (item ID to start after).
    pub after: Option<String>,

    /// Sort order for items (default: desc). Ascending order is oldest-first.
    #[serde(default)]
    pub order: ItemOrder,

    /// Optional output expansion, encoded as `include[]=value` by `OpenAI` clients.
    #[serde(alias = "include[]")]
    pub include: Option<ItemInclude>,
}

impl ListItemsQuery {
    /// Decode item listing parameters while retaining parameter-specific errors.
    ///
    /// # Errors
    /// Returns an OpenAI-compatible 400 for invalid or duplicate query parameters.
    pub(super) fn parse(raw: Option<&str>) -> Result<Self, Box<Response>> {
        let invalid = |param: &str, code: Option<&'static str>, message: String| {
            Box::new(executor_error_response(ExecutorError::UnsupportedValue {
                param: param.to_owned(),
                code,
                message,
            }))
        };
        let mut query = Self::default();
        let mut seen = HashSet::new();
        for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
            let key = match key.as_ref() {
                "include" | "include[]" => "include",
                "limit" => "limit",
                "order" => "order",
                "after" => "after",
                _ => continue,
            };
            // Check each expansion, including repeated includes, before rejecting duplicates.
            if key == "include" {
                let include = value
                    .parse::<ItemInclude>()
                    .map_err(|error| invalid("include[0]", Some("invalid_value"), error.to_string()))?;
                if include == ItemInclude::ReasoningEncryptedContent {
                    return Err(invalid(
                        "include",
                        None,
                        "Encrypted content cannot be requested for persisted responses.".into(),
                    ));
                }
                query.include = Some(include);
            }
            if !seen.insert(key) {
                return Err(invalid(
                    key,
                    Some("duplicate_parameter"),
                    format!("'{key}' must be given at most once"),
                ));
            }
            match key {
                "limit" => {
                    let limit = value
                        .parse::<i64>()
                        .map_err(|_| invalid(key, Some("invalid_type"), "limit must be an integer".into()))?;
                    query.limit = ItemLimit::try_from(limit)
                        .map_err(|error| invalid(key, Some(error.code()), error.to_string()))?;
                }
                "order" => {
                    let order: Result<ItemOrder, serde::de::value::Error> =
                        ItemOrder::deserialize(value.as_ref().into_deserializer());
                    query.order = order.map_err(|error| invalid(key, Some("invalid_value"), error.to_string()))?;
                }
                "after" => query.after = Some(value.into_owned()),
                _ => {}
            }
        }
        Ok(query)
    }

    /// Borrow validated pagination arguments for the executor API.
    pub(super) fn page(&self) -> ItemPage<'_> {
        ItemPage {
            limit: self.limit,
            after: self.after.as_deref(),
            order: self.order,
        }
    }
}
