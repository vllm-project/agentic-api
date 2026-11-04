//! Compatibility with Claude Code's full-field and inherited-field continuations.
//!
//! Its billing attribution is per-request metadata carried inside `system`, not
//! part of the retained prompt. Only the reserved, single-line text block is
//! removed. All other prompt and tool fields must equal their stored values.

use std::collections::BTreeMap;

use serde_json::Value;
use serde_json::value::RawValue;

use super::MessagesThreadError;
use crate::types::messages::thread::{CLAUDE_CODE_BETA, MessageThreadErrorCode, ThreadMessagesRequest};

pub(super) fn enabled(betas: &[String]) -> bool {
    betas.iter().any(|beta| beta == CLAUDE_CODE_BETA)
}

pub(super) fn normalize_attribution(
    parameters: &mut BTreeMap<String, Box<RawValue>>,
) -> Result<(), MessagesThreadError> {
    let Some(raw) = parameters.get("system") else {
        return Ok(());
    };
    let mut system: Value = serde_json::from_str(raw.get()).map_err(|error| invalid_json(&error))?;
    let Some(blocks) = system.as_array_mut() else {
        return Ok(());
    };
    let original_len = blocks.len();
    blocks.retain(|block| !is_attribution(block));
    if blocks.len() == original_len {
        return Ok(());
    }
    if blocks.is_empty() {
        parameters.remove("system");
    } else {
        parameters.insert(
            "system".to_owned(),
            serde_json::value::to_raw_value(&system).map_err(|error| invalid_json(&error))?,
        );
    }
    Ok(())
}

pub(super) fn validate_repeated_fields(
    request: &ThreadMessagesRequest,
    stored: &BTreeMap<String, Box<RawValue>>,
) -> Result<(), MessagesThreadError> {
    let mut changed = Vec::new();
    for field in ["system", "tools"] {
        let Some(current) = request.parameters.get(field) else {
            continue;
        };
        let current: Value = serde_json::from_str(current.get()).map_err(|error| invalid_json(&error))?;
        if current.is_null() {
            return Err(MessagesThreadError::invalid(format!(
                "{field} must not be null on thread continuation"
            )));
        }
        let previous = stored
            .get(field)
            .map(|raw| serde_json::from_str::<Value>(raw.get()))
            .transpose()
            .map_err(|error| invalid_json(&error))?;
        if previous.as_ref() != Some(&current) {
            changed.push(field);
        }
    }
    if changed.is_empty() {
        Ok(())
    } else {
        Err(MessagesThreadError::coded(
            MessageThreadErrorCode::ThreadFingerprintMismatch,
            format!(
                "thread fingerprint mismatch; differing field(s): {}",
                changed.join(", ")
            ),
        ))
    }
}

fn is_attribution(block: &Value) -> bool {
    let Some(object) = block.as_object() else {
        return false;
    };
    if object.len() != 2 || block["type"] != "text" {
        return false;
    }
    let Some(text) = block["text"].as_str() else {
        return false;
    };
    let Some(metadata) = text.strip_prefix("x-anthropic-billing-header: ") else {
        return false;
    };
    // Do not discard multiline text, extensions, or arbitrary prefix lookalikes.
    if !metadata.is_ascii() || metadata.bytes().any(|byte| byte.is_ascii_control()) {
        return false;
    }
    let mut version = false;
    for field in metadata.split(';').map(str::trim).filter(|field| !field.is_empty()) {
        let Some((key, value)) = field.split_once('=') else {
            return false;
        };
        if key.is_empty()
            || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || value.is_empty()
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return false;
        }
        version |= key == "cc_version";
    }
    version
}

fn invalid_json(error: &serde_json::Error) -> MessagesThreadError {
    MessagesThreadError::invalid(format!("invalid Claude Code thread field: {error}"))
}
