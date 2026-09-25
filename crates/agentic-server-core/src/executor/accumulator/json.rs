//! Validation for complete JSON response bodies.

use crate::events::ensure_supported_output_item_type;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::OutputItem;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

use super::{ResponseAccumulator, StreamLifecycle, Validation};
use crate::executor::response_budget::{RetainedSize, retained_response_parts_bytes};
use crate::types::event::ResponseStatus;
use crate::types::io::ResponseUsage;
use crate::types::request_response::IncompleteDetails;
use crate::types::upstream_identity::{UpstreamModelError, UpstreamResponseIdentity};
use crate::utils::common::{deserialize_from_str, deserialize_from_value_opt};

impl ResponseAccumulator {
    pub(in crate::executor) fn load_json_body(&mut self, body: &str) -> ExecutorResult<()> {
        let acc = Self::read_json(body, self.conversation_id.clone(), self.validation)?;
        let retained = retained_response_parts_bytes(&acc.response_id, &acc.output)
            + acc.incomplete_details.retained_bytes()
            + acc.error.retained_bytes()
            + acc.upstream_model_retained_bytes();
        if let Some(budget) = &self.budget {
            budget.consume(retained)?;
        }
        let budget = self.budget.clone();
        *self = acc;
        self.budget = budget;
        Ok(())
    }

    pub(super) fn read_json(
        body: &str,
        conversation_id: Option<String>,
        validation: Validation,
    ) -> ExecutorResult<Self> {
        let mut json: Value = deserialize_from_str(body).map_err(ExecutorError::JsonError)?;
        if validation == Validation::Strict {
            ensure_strict_response(&json)?;
        }
        let identity = UpstreamResponseIdentity::observe(&json);
        if identity.invalid_model && validation == Validation::Strict {
            return Err(UpstreamModelError::Invalid.into());
        }
        let response_id = json["id"]
            .as_str()
            .ok_or_else(|| ExecutorError::ParseError("missing 'id' field in response".into()))?
            .to_owned();
        let mut acc = Self::with_validation(response_id, conversation_id, validation);
        acc.model_evidence_invalidated = identity.invalid_model;
        acc.terminal_model_reported =
            identity.model.is_some() && matches!(json["status"].as_str(), Some("completed" | "failed" | "incomplete"));
        acc.upstream_model = identity.model;
        acc.output = deserialize_from_value_opt::<Vec<Value>>(json["output"].take())
            .map(|items| {
                let mut out = Vec::with_capacity(items.len());
                out.extend(items.into_iter().filter_map(deserialize_from_value_opt::<OutputItem>));
                out
            })
            .unwrap_or_default();
        acc.status = json["status"]
            .as_str()
            .map_or(ResponseStatus::Completed, |s| s.parse().unwrap_or_default());
        acc.usage = deserialize_from_value_opt::<ResponseUsage>(json["usage"].take());
        acc.incomplete_details = deserialize_from_value_opt::<IncompleteDetails>(json["incomplete_details"].take());
        acc.error = (!json["error"].is_null()).then(|| json["error"].take());
        acc.stream_lifecycle = StreamLifecycle::Terminal;
        Ok(acc)
    }
}

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
