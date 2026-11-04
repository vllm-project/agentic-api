//! Decoding, validation, and retained-byte accounting for complete JSON response bodies.

use super::slot::SlotMap;
use super::{ResponseAccumulator, StreamLifecycle, Validation};
use crate::events::{UpstreamResponse, UpstreamStatus};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{RetainedAccount, RetainedSize, retained_response_parts_bytes};
use crate::types::event::ResponseStatus;
use crate::types::io::{OutputItem, ResponseUsage};
use crate::types::request_response::IncompleteDetails;
use crate::utils::common::{deserialize_from_str, deserialize_from_value_opt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Checks the complete JSON response contract for strict ingestion: the typed response object with a terminal status,
/// typed output items of supported types, and unique item IDs.
///
/// # Errors
/// [`ExecutorError::InvalidRequest`] naming what is missing or invalid.
pub(super) fn ensure_strict_response(json: &Value) -> ExecutorResult<()> {
    let invalid = ExecutorError::InvalidRequest;
    let response = UpstreamResponse::decode(json).map_err(|error| invalid(error.to_string()))?;
    if response.status == UpstreamStatus::InProgress {
        return Err(invalid(
            "upstream response status 'in_progress' is not terminal".to_owned(),
        ));
    }
    let output = Vec::<OutputItem>::deserialize(&json["output"])
        .map_err(|error| invalid(format!("upstream response output is invalid: {error}")))?;
    let mut item_ids = HashSet::with_capacity(output.len());
    for (index, item) in output.iter().enumerate() {
        if matches!(item, OutputItem::Unknown) {
            return Err(invalid(format!(
                "upstream response output[{index}] has an unsupported type"
            )));
        }
        let item_id = item
            .id()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid(format!("upstream response output[{index}] has no valid 'id'")))?;
        if !item_ids.insert(item_id) {
            return Err(invalid(format!("upstream response repeats output item '{item_id}'")));
        }
    }
    Ok(())
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
            // Lenient ingestion drops output items the typed schema rejects; strict ingestion rejected them above.
            .map(|items| {
                items
                    .into_iter()
                    .filter_map(deserialize_from_value_opt::<OutputItem>)
                    .collect()
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
