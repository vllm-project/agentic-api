//! Decoding, validation, and retained-byte accounting for complete JSON response bodies.

use super::slot::SlotMap;
use super::{ResponseAccumulator, StreamLifecycle, Validation};
use crate::events::ensure_supported_output_item_type;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{RetainedAccount, RetainedSize, retained_response_parts_bytes};
use crate::types::event::ResponseStatus;
use crate::types::io::{OutputItem, ReasoningOutput, ResponseUsage};
use crate::types::request_response::IncompleteDetails;
use crate::utils::common::{deserialize_from_str, deserialize_from_value_opt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

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
    if json
        .get("service_tier")
        .is_some_and(|tier| !tier.is_null() && !tier.is_string())
    {
        return Err(missing_field("upstream response", "service_tier"));
    }
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

/// Decodes one upstream output item under lenient ingestion, dropping unreadable items.
///
/// Earlier releases relayed reasoning with untyped fields. Keep such an item with
/// the fields the typed schema accepts instead of dropping it; strict ingestion
/// rejects it in [`ensure_strict_response`].
pub(super) fn lenient_output_item(item: Value) -> Option<OutputItem> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return deserialize_from_value_opt(item);
    }
    OutputItem::deserialize(&item)
        .ok()
        .or_else(|| ReasoningOutput::from_legacy_value(&item).map(OutputItem::Reasoning))
}

impl ResponseAccumulator {
    /// Parses a non-streaming JSON response body.
    ///
    /// # Errors
    /// Returns `ExecutorError::ParseError` if JSON parsing fails or required fields are missing.
    pub fn from_json(body: &str, conversation_id: Option<&str>) -> ExecutorResult<Self> {
        Self::read_json(body, conversation_id.map(str::to_owned), Validation::Lenient)
    }

    pub(in crate::executor) fn load_json_body(&mut self, body: &str) -> ExecutorResult<()> {
        let acc = Self::read_json(body, self.conversation_id.clone(), self.validation)?;
        let retained = retained_response_parts_bytes(&acc.response_id, &acc.output, acc.service_tier.as_deref())
            + acc.incomplete_details.retained_bytes()
            + acc.error.retained_bytes();
        if let Some(budget) = &self.budget {
            budget.consume(retained)?;
        }
        let budget = self.budget.clone();
        *self = acc;
        self.budget = budget;
        Ok(())
    }

    fn read_json(body: &str, conversation_id: Option<String>, validation: Validation) -> ExecutorResult<Self> {
        let mut json: serde_json::Value = deserialize_from_str(body).map_err(ExecutorError::JsonError)?;
        if validation == Validation::Strict {
            ensure_strict_response(&json)?;
        }

        let response_id = json["id"]
            .as_str()
            .ok_or_else(|| ExecutorError::ParseError("missing 'id' field in response".into()))?
            .to_string();

        let output = deserialize_from_value_opt::<Vec<serde_json::Value>>(json["output"].take())
            .map(|items| {
                let mut out = Vec::with_capacity(items.len());
                out.extend(items.into_iter().filter_map(lenient_output_item));
                out
            })
            .unwrap_or_default();

        let status = json["status"]
            .as_str()
            .map_or(ResponseStatus::Completed, |s| s.parse().unwrap_or_default());

        let usage = deserialize_from_value_opt::<ResponseUsage>(json["usage"].take());
        let service_tier = json["service_tier"].as_str().map(str::to_owned);
        let incomplete_details = deserialize_from_value_opt::<IncompleteDetails>(json["incomplete_details"].take());
        let error = (!json["error"].is_null()).then(|| json["error"].take());

        Ok(Self {
            validation,
            response_id,
            conversation_id,
            output,
            usage,
            service_tier,
            service_tier_account: RetainedAccount::default(),
            status,
            incomplete_details,
            error,
            terminal_details_account: RetainedAccount::default(),
            slots: SlotMap::default(),
            strict_call_ids: HashMap::new(),
            stream_lifecycle: StreamLifecycle::Terminal,
            budget: None,
        })
    }
}
