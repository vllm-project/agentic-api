//! Thread-specific validation and fixed-field comparison.

use std::collections::BTreeMap;

use serde_json::Value;

use super::MessagesThreadError;
use crate::types::messages::thread::{MESSAGE_THREADS_BETA, MessageThreadParam, ThreadMessagesRequest};

pub(super) fn fingerprint(request: &ThreadMessagesRequest) -> Result<BTreeMap<String, Value>, MessagesThreadError> {
    let mut fixed = BTreeMap::new();
    fixed.insert("model".to_owned(), Value::String(request.model.clone()));
    for field in [
        "tool_choice",
        "thinking",
        "mcp_servers",
        "context_management",
        "output_config",
        "cache_control",
    ] {
        if let Some(raw) = request.parameters.get(field) {
            let mut value: Value =
                serde_json::from_str(raw.get()).map_err(|e| MessagesThreadError::invalid(e.to_string()))?;
            if field == "mcp_servers"
                && let Some(servers) = value.as_array_mut()
            {
                for server in servers {
                    if let Some(server) = server.as_object_mut() {
                        server.remove("authorization_token");
                    }
                }
            }
            // Omitted and null fixed parameters have the same default meaning.
            if !value.is_null() {
                fixed.insert(field.to_owned(), value);
            }
        }
    }
    if let Some(raw) = request.parameters.get("container") {
        let value: Value = serde_json::from_str(raw.get()).map_err(|e| MessagesThreadError::invalid(e.to_string()))?;
        if let Some(skills) = value.get("skills") {
            fixed.insert("container.skills".to_owned(), skills.clone());
        }
    }
    fixed.insert(
        "fallbacks".to_owned(),
        Value::Bool(request.parameters.contains_key("fallbacks")),
    );
    Ok(fixed)
}

pub(super) fn validate_request(request: &ThreadMessagesRequest, betas: &[String]) -> Result<(), MessagesThreadError> {
    if !betas.iter().any(|beta| beta == MESSAGE_THREADS_BETA) {
        return Err(MessagesThreadError::invalid(format!(
            "thread requires the {MESSAGE_THREADS_BETA} beta header"
        )));
    }
    if request.model.is_empty() || request.max_tokens == 0 {
        return Err(MessagesThreadError::invalid(
            "thread requires a model and positive max_tokens",
        ));
    }
    if matches!(request.thread, MessageThreadParam::Continue { .. })
        && !super::claude_code::enabled(betas)
        && ["system", "tools"]
            .iter()
            .any(|field| request.parameters.contains_key(*field))
    {
        return Err(MessagesThreadError::invalid(
            "system and tools must be omitted on thread continuation, including null",
        ));
    }
    if matches!(request.thread, MessageThreadParam::Create) && request.messages.is_empty() {
        return Err(MessagesThreadError::invalid("thread create requires messages"));
    }
    if let MessageThreadParam::Continue { previous_message_id } = &request.thread
        && previous_message_id.is_empty()
    {
        return Err(MessagesThreadError::invalid("previous_message_id must not be empty"));
    }
    let value = serde_json::to_value(request).map_err(|e| MessagesThreadError::invalid(e.to_string()))?;
    if value.get("compaction").is_some_and(|value| !value.is_null()) {
        return Err(MessagesThreadError::unsupported("context_management"));
    }
    if let Some(management) = value.get("context_management").filter(|value| !value.is_null()) {
        let supported = management.get("edits").and_then(Value::as_array).is_some_and(|edits| {
            edits
                .iter()
                .all(|edit| edit["type"] == "clear_thinking_20251015" && edit["keep"] == "all")
        });
        if !supported {
            return Err(MessagesThreadError::unsupported("context_management"));
        }
    }
    if betas.iter().any(|beta| beta == "compact-2026-09-04") && has_compaction(&value["messages"]) {
        return Err(MessagesThreadError::unsupported("context_management"));
    }
    if cache_breakpoints(&value) > 4 {
        return Err(MessagesThreadError::unsupported("cache_control"));
    }
    Ok(())
}

pub(super) fn cache_breakpoints(value: &Value) -> usize {
    match value {
        Value::Array(values) => values.iter().map(cache_breakpoints).sum(),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                usize::from(key == "cache_control" && !value.is_null())
                    + if key == "cache_control" || key == "input" || key == "input_schema" {
                        0
                    } else {
                        cache_breakpoints(value)
                    }
            })
            .sum(),
        _ => 0,
    }
}

fn has_compaction(value: &Value) -> bool {
    value.as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "compaction"))
        })
    })
}
