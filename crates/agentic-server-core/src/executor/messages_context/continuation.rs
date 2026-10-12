//! Resumed MCP outputs and fulfilled tool-choice transitions for Messages history.

use super::MessagesRequestContext;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::messages::{GatewayToolResult, MessagesToolChoice};

impl MessagesRequestContext {
    pub(in crate::executor) fn take_pending_mcp(&mut self) -> Vec<crate::types::messages::mcp::PendingMcpCall> {
        std::mem::take(&mut self.pending_mcp)
    }

    /// Add resumed outputs to the existing client-result message, without repeating calls.
    pub(in crate::executor) fn complete_pending_mcp(&mut self, results: &[GatewayToolResult]) -> ExecutorResult<()> {
        let fulfilled_choice = match self.tool_choice.as_ref() {
            Some(MessagesToolChoice::Any(_)) => !results.is_empty(),
            Some(MessagesToolChoice::Tool { name, .. }) => self.raw["messages"]
                .as_array()
                .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "assistant"))
                .and_then(|message| message["content"].as_array())
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["type"] == "tool_use"
                            && block["name"] == name.as_str()
                            && results.iter().any(|result| block["id"] == result.tool_use_id)
                    })
                }),
            Some(MessagesToolChoice::Auto(_) | MessagesToolChoice::None { .. }) | None => false,
        };
        let content = self.raw["messages"]
            .as_array_mut()
            .and_then(|messages| messages.last_mut())
            .and_then(|message| message["content"].as_array_mut())
            .ok_or_else(|| {
                ExecutorError::InvalidRequest("pending MCP continuation has no client results".to_owned())
            })?;
        for result in results {
            content.push(serde_json::to_value(result).map_err(ExecutorError::JsonError)?);
        }
        self.relax_fulfilled_choice(fulfilled_choice)
    }

    pub(super) fn relax_fulfilled_choice(&mut self, fulfilled_choice: bool) -> ExecutorResult<()> {
        if fulfilled_choice {
            if let Some(choice) = &mut self.tool_choice {
                match choice {
                    MessagesToolChoice::Any(options) | MessagesToolChoice::Tool { options, .. } => {
                        *choice = MessagesToolChoice::Auto(std::mem::take(options));
                    }
                    MessagesToolChoice::Auto(_) | MessagesToolChoice::None { .. } => {}
                }
                self.raw["tool_choice"] = serde_json::to_value(choice).map_err(ExecutorError::JsonError)?;
            }
        }
        Ok(())
    }
}
