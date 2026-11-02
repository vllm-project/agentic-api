//! Native server tool calls as the Messages client sees them.
//!
//! When the client declares Anthropic's native `web_search` or `web_fetch`, the
//! gateway runs each call itself and returns it the way Anthropic does: a
//! `server_tool_use` block and its result block in the assistant turn. The
//! model's own rounds are unchanged — a `tool_use` and a `tool_result` — so
//! later turns convert the blocks back (`replay`).
//!
//! The result block is the record of a call. The `tool_result` text the model
//! sees is always projected from it, on the live round and on replay, so a
//! replayed conversation gives the upstream the same text as the original
//! rounds. Search page content travels sealed in `encrypted_content`
//! (`sealing`); a fetched page, and the reason a fetch failed, travel in the
//! block's plain fields. Search failures show the documented code, and the
//! executor's detail is logged.

mod model_text;
mod projection;
pub(crate) mod replay;
pub(crate) mod sealing;

use serde_json::Value;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::messages_usage::ServerToolUses;
use crate::types::messages::{GatewayToolMap, GatewayToolResult, ServerToolResultBlock, ServerToolUseBlock, ToolParam};
pub(super) use model_text::fetch_error_message;
pub(super) use projection::{project_native_call, replayed_tool_result};
use sealing::MessagesResultKey;

/// Prefix of the `server_tool_use` id the client sees for a gateway call; the
/// rest is the upstream `tool_use` id, restored on replay.
const CLIENT_ID_PREFIX: &str = "srvtoolu_gw_";

/// The client-visible id of the gateway call with upstream id `upstream_id`.
pub(super) fn client_tool_use_id(upstream_id: &str) -> String {
    format!("{CLIENT_ID_PREFIX}{upstream_id}")
}

/// The upstream id behind a client-visible id, if the gateway made it.
pub(super) fn upstream_tool_use_id(client_id: &str) -> Option<&str> {
    client_id.strip_prefix(CLIENT_ID_PREFIX)
}

/// Whether `body` may carry a gateway-made server tool block, judged without
/// parsing it.
#[must_use]
pub fn may_carry_gateway_server_tool_blocks(body: &[u8]) -> bool {
    let prefix = CLIENT_ID_PREFIX.as_bytes();
    body.windows(prefix.len()).any(|window| window == prefix)
}

/// A native server tool the gateway runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NativeTool {
    WebSearch,
    WebFetch,
}

/// The native server tools a request declared. Only their calls are shown to
/// the client; a call through an operator alias or a plain function stays the
/// gateway's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct NativeServerTools {
    web_search: bool,
    web_fetch: bool,
}

impl NativeServerTools {
    pub(super) fn from_declarations(tools: Option<&Vec<ToolParam>>) -> Self {
        tools
            .into_iter()
            .flatten()
            .fold(Self::default(), |native, tool| match tool {
                ToolParam::WebSearch(tool) if tool.fields.name == "web_search" => Self {
                    web_search: true,
                    ..native
                },
                ToolParam::WebFetch(_) => Self {
                    web_fetch: true,
                    ..native
                },
                ToolParam::WebSearch(_) | ToolParam::Function(_) | ToolParam::Provider(_) => native,
            })
    }

    /// The native tool a gateway call to `name` runs, if the client declared it natively.
    pub(super) fn tool(self, name: &str) -> Option<NativeTool> {
        match name {
            "web_search" if self.web_search => Some(NativeTool::WebSearch),
            "web_fetch" if self.web_fetch => Some(NativeTool::WebFetch),
            _ => None,
        }
    }

    /// The `server_tool_use` block the client sees for a gateway `tool_use`
    /// block of the model's turn, or `None` for a call that stays hidden.
    pub(super) fn server_tool_use(self, block: &Value) -> Option<ServerToolUseBlock> {
        let name = block.get("name").and_then(Value::as_str)?;
        self.tool(name)?;
        let id = block.get("id").and_then(Value::as_str)?;
        Some(ServerToolUseBlock::new(
            client_tool_use_id(id),
            name.to_owned(),
            block.get("input").cloned().unwrap_or_default(),
        ))
    }

    /// What the client sees of one executed round: the model's blocks in
    /// order, each native call as its `server_tool_use` block and any other
    /// gateway call hidden, then the native calls' result blocks.
    ///
    /// # Errors
    /// Returns [`ExecutorError::JsonError`] if a block cannot be serialized.
    pub(super) fn executed_round<'b>(
        self,
        content: &[Value],
        gateway_map: &GatewayToolMap,
        results: impl IntoIterator<Item = &'b ServerToolResultBlock>,
    ) -> ExecutorResult<Vec<Value>> {
        let mut shown = Vec::with_capacity(content.len());
        for block in content {
            let gateway_call = block.get("type").and_then(Value::as_str) == Some("tool_use")
                && gateway_map.is_gateway_owned(block.get("name").and_then(Value::as_str).unwrap_or_default());
            if !gateway_call {
                shown.push(block.clone());
            } else if let Some(call) = self.server_tool_use(block) {
                shown.push(serde_json::to_value(call).map_err(ExecutorError::JsonError)?);
            }
        }
        for result in results {
            shown.push(serde_json::to_value(result).map_err(ExecutorError::JsonError)?);
        }
        Ok(shown)
    }
}

/// How one request's gateway calls reach its client: which calls are native
/// server tool calls, and the key that seals their search results.
#[derive(Clone, Copy, Debug)]
pub(super) struct ServerToolView<'a> {
    pub(super) native: NativeServerTools,
    pub(super) key: &'a MessagesResultKey,
}

/// What a native call came to, before it is shown to the client.
#[derive(Debug)]
pub(super) enum NativeOutcome {
    /// The executor answered; `failed` is its output status.
    Output { text: String, failed: bool },
    /// Answered without running.
    Refused(Refusal),
    /// Could not run: no handler, an execution error, or a timeout.
    Failed,
}

/// Why a native call was answered without running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    InvalidInput,
    MaxUsesExceeded,
    UrlNotInPriorContext,
}

/// A native call's result block, the `tool_result` projected from it, and the
/// server tool uses the call performed.
#[derive(Debug)]
pub(super) struct ProjectedResult {
    pub(super) block: ServerToolResultBlock,
    pub(super) tool_result: GatewayToolResult,
    pub(super) uses: ServerToolUses,
}

/// Why a replayed result block cannot be given back to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplayFailure {
    /// Sealed content was altered, malformed, or does not match its call.
    Tampered,
    /// The call's input no longer parses as the search it ran.
    InvalidInput,
}

#[cfg(test)]
mod tests;
