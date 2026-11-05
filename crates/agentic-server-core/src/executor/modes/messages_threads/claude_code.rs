//! Compatibility with Claude Code's full-field and inherited-field continuations.
//!
//! Its billing attribution is per-request metadata carried inside `system`, not
//! part of the retained prompt. Only the reserved, single-line text block is
//! removed. All other prompt and tool fields must equal their stored values.

use super::MessagesThreadError;
use crate::types::messages::parameters::RequestField;
use crate::types::messages::thread::{CLAUDE_CODE_BETA, MessageThreadCheckpoint, MessageThreadErrorCode};
use crate::types::messages::{MessagesRequest, SystemBlock, SystemPrompt};

pub(super) fn enabled(betas: &[String]) -> bool {
    betas.iter().any(|beta| beta == CLAUDE_CODE_BETA)
}

pub(super) fn normalize_attribution(system: &mut RequestField<SystemPrompt>) {
    if let RequestField::Value(SystemPrompt::Blocks(blocks)) = system {
        let original_len = blocks.len();
        blocks.retain(|block| !is_attribution(block));
        if blocks.len() != original_len && blocks.is_empty() {
            *system = RequestField::Missing;
        }
    }
}

pub(super) fn validate_repeated_fields(
    request: &MessagesRequest,
    stored: &MessageThreadCheckpoint,
) -> Result<(), MessagesThreadError> {
    if request.system.is_null() || request.tools.is_null() {
        return Err(MessagesThreadError::invalid(
            "system and tools must not be null on thread continuation",
        ));
    }
    let mut changed = Vec::new();
    if !request.system.is_missing() && request.system.as_ref() != stored.system.as_ref() {
        changed.push("system");
    }
    if !request.tools.is_missing() && request.tools.as_ref() != stored.tools.as_ref() {
        changed.push("tools");
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

fn is_attribution(block: &SystemBlock) -> bool {
    if block.kind.as_ref().map(String::as_str) != Some("text")
        || !block.cache_control.is_missing()
        || !block.extra.is_empty()
    {
        return false;
    }
    let text = &block.text;
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
