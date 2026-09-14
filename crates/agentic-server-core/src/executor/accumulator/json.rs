//! Validation for complete JSON response bodies.

use crate::events::ensure_supported_output_item_type;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::OutputItem;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

fn required_str<'a>(value: &'a Value, field: &str, owner: &str) -> ExecutorResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_field(owner, field))
}

fn missing_field(owner: &str, field: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(format!("{owner} has no valid '{field}'"))
}

/// Checks the complete JSON response contract for strict ingestion:
/// terminal status, readable output items, and unique item identifiers.
///
/// # Errors
/// [`ExecutorError::InvalidRequest`] naming the field that is missing or invalid.
pub(super) fn ensure_strict_response(json: &Value) -> ExecutorResult<()> {
    let Some(status) = json["status"].as_str() else {
        return Err(ExecutorError::InvalidRequest(
            "upstream response has no 'status'".to_owned(),
        ));
    };
    if !matches!(status, "completed" | "failed" | "incomplete") {
        return Err(ExecutorError::InvalidRequest(format!(
            "upstream response status '{status}' is not terminal"
        )));
    }
    let Some(items) = json["output"].as_array() else {
        return Err(ExecutorError::InvalidRequest(
            "upstream response has no 'output' array".to_owned(),
        ));
    };
    let mut item_ids = HashSet::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let owner = format!("upstream response output[{index}]");
        let item_id = required_str(item, "id", &owner)?;
        let item_type = required_str(item, "type", &owner)?;
        ensure_supported_output_item_type(item_type)
            .map_err(|error| ExecutorError::InvalidRequest(error.to_string()))?;
        OutputItem::deserialize(item).map_err(|error| {
            ExecutorError::InvalidRequest(format!(
                "upstream response output[{index}] is not a valid item: {error}"
            ))
        })?;
        if !item_ids.insert(item_id) {
            return Err(ExecutorError::InvalidRequest(format!(
                "upstream response repeats output item '{item_id}'"
            )));
        }
    }
    Ok(())
}
