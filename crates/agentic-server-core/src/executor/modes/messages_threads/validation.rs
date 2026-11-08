//! Thread admission over the shared typed Messages request.

use super::{MessagesThreadError, claude_code};
use crate::types::messages::ContentBlock;
use crate::types::messages::parameters::{ContextEdit, ThinkingKeep};
use crate::types::messages::thread::{MESSAGE_THREADS_BETA, MessageThreadParam};
use crate::types::messages::{MessageContent, MessagesRequest, SystemPrompt};

pub(super) fn validate_request(request: &MessagesRequest, betas: &[String]) -> Result<(), MessagesThreadError> {
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
    if request
        .messages
        .iter()
        .any(|message| !matches!(message.role.as_str(), "user" | "assistant"))
    {
        return Err(MessagesThreadError::invalid(
            "thread messages require a user or assistant role",
        ));
    }
    if matches!(request.thread, Some(MessageThreadParam::Continue { .. }))
        && !claude_code::enabled(betas)
        && (!request.system.is_missing() || !request.tools.is_missing())
    {
        return Err(MessagesThreadError::invalid(
            "system and tools must be omitted on thread continuation, including null",
        ));
    }
    if matches!(request.thread, Some(MessageThreadParam::Create)) && request.messages.is_empty() {
        return Err(MessagesThreadError::invalid("thread create requires messages"));
    }
    if let Some(MessageThreadParam::Continue { previous_message_id }) = &request.thread
        && previous_message_id.is_empty()
    {
        return Err(MessagesThreadError::invalid("previous_message_id must not be empty"));
    }
    if request.compaction.is_some() {
        return Err(MessagesThreadError::unsupported("context_management"));
    }
    if let Some(management) = &request.context_management
        && !management.edits.as_ref().is_some_and(|edits| {
            edits.iter().all(|edit| {
                matches!(
                    edit,
                    ContextEdit::ClearThinking {
                        keep: Some(ThinkingKeep::All(_)),
                        ..
                    }
                )
            })
        })
    {
        return Err(MessagesThreadError::unsupported("context_management"));
    }
    if betas.iter().any(|beta| beta == "compact-2026-09-04") && request.messages.iter().any(|message| {
        matches!(&message.content, MessageContent::Blocks(blocks) if blocks.iter().any(ContentBlock::is_compaction))
    }) { return Err(MessagesThreadError::unsupported("context_management")); }
    if cache_breakpoints(request) > 4 {
        return Err(MessagesThreadError::unsupported("cache_control"));
    }
    Ok(())
}

pub(super) fn cache_breakpoints(request: &MessagesRequest) -> usize {
    usize::from(request.cache_control.is_some())
        + match request.system.as_ref() {
            Some(SystemPrompt::Blocks(blocks)) => blocks
                .iter()
                .filter(|block| block.cache_control.as_ref().is_some())
                .count(),
            Some(SystemPrompt::Text(_)) | None => 0,
        }
        + request.tools.as_ref().map_or(0, |tools| {
            tools
                .iter()
                .filter(|tool| tool.cache_control().as_ref().is_some())
                .count()
        })
        + request
            .messages
            .iter()
            .map(|message| match &message.content {
                MessageContent::Text(_) => 0,
                MessageContent::Blocks(blocks) => blocks.iter().map(ContentBlock::cache_breakpoints).sum(),
            })
            .sum::<usize>()
}
