//! Explicit request-scoped tool preparation after public rehydration.

use crate::executor::error::ExecutorResult;
use crate::executor::rehydrate::apply_effective_settings;
use crate::executor::request::RequestContext;
use crate::tool::{ToolSearchHandler, ToolSearchState};
use crate::types::tools::ResponsesTool;

/// Prepare the tool-search projection for a fully rehydrated public request.
///
/// Compaction may remove the call/output pair that records which deferred definitions were loaded; the metadata
/// rehydration loaded for the continued turn still records them, so preparation reads no storage.
///
/// # Errors
/// Returns an error when the public tool-search history or effective tool selection is invalid.
pub(crate) fn prepare_request_tools(
    mut ctx: RequestContext,
) -> ExecutorResult<(RequestContext, Option<ToolSearchState>)> {
    if ctx
        .enriched_request
        .multi_agent
        .as_ref()
        .is_some_and(|config| config.enabled)
    {
        // Each agent prepares its own search view from its canonical checkpoint.
        return Ok((ctx, None));
    }
    let restored_loaded_tools = restored_loaded_tools(&mut ctx);
    let restore_only_declared = ctx.original_request.tools.is_some();
    let state =
        ToolSearchHandler::prepare_request(&mut ctx.enriched_request, &restored_loaded_tools, restore_only_declared)?;
    Ok((ctx, state))
}

fn restored_loaded_tools(ctx: &mut RequestContext) -> Vec<ResponsesTool> {
    if !ctx.enriched_request.input.contains_compaction() {
        return Vec::new();
    }
    let Some(loaded) = ctx
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.metadata.tool_search_loaded_tools.clone())
    else {
        return Vec::new();
    };
    // A response continuation already applied its parent's settings during rehydration; a conversation turn
    // applies them only when compaction made it depend on the loaded definitions.
    if ctx.original_request.conversation.is_some()
        && let Some(checkpoint) = ctx.checkpoint.take()
    {
        apply_effective_settings(ctx, &checkpoint.metadata);
        ctx.checkpoint = Some(checkpoint);
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::session::ResponseCheckpoint;
    use crate::storage::ResponseMetadata;
    use crate::types::request_response::RequestPayload;

    #[test]
    fn compacted_session_restores_loaded_tools_without_durable_storage() {
        let loaded: ResponsesTool = serde_json::from_value(serde_json::json!({
            "type": "function", "name": "weather", "parameters": {"type": "object"}
        }))
        .unwrap();
        // The session parent as rehydration records it: not durable, history not kept on the checkpoint.
        let checkpoint = ResponseCheckpoint {
            response_id: "resp_parent".to_owned(),
            history: Vec::new(),
            metadata: ResponseMetadata {
                tool_search_loaded_tools: Some(vec![loaded.clone()]),
                ..ResponseMetadata::default()
            },
            durable: false,
        };
        let request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test", "store": false, "previous_response_id": "resp_parent",
            "input": [{"type": "compaction", "id": "cmp_1", "encrypted_content": "summary"}]
        }))
        .unwrap();
        let mut ctx = RequestContext {
            multi_agent_tree: None,
            original_request: request.clone(),
            enriched_request: request,
            new_input_items: Vec::new(),
            response_id: "resp_child".to_owned(),
            conversation_id: None,
            checkpoint: Some(checkpoint),
            continuation: None,
        };
        let restored = restored_loaded_tools(&mut ctx);
        assert_eq!(serde_json::to_value(restored).unwrap(), serde_json::json!([loaded]));
    }
}
