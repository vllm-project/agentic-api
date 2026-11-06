//! Typed public item identities and their known type-specific prefixes.

use thiserror::Error;

use crate::types::client_calls::ClientCallId;
use crate::types::event::MessageStatus;
use crate::types::io::{
    FunctionToolCall, InOutItem, InputItem, OutputItem, OutputMessage, ReasoningOutput, ShellCallStatus,
};

/// A client-supplied item `OpenAI` refuses, named by its parameter path and worded as `OpenAI` words it.
#[derive(Debug, Error)]
pub enum ClientItemError {
    /// The item's ID does not begin with its type's prefix.
    #[error("Invalid '{param}': '{id}'. Expected an ID that begins with '{prefix}'.")]
    InvalidId {
        param: String,
        id: String,
        prefix: &'static str,
    },
    /// A call or an output has an empty `call_id`.
    #[error(
        "Invalid '{param}': empty string. Expected a string with minimum length 1, but got an empty string instead."
    )]
    EmptyString { param: String },
}

impl ClientItemError {
    /// The error envelope's `code`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "invalid_value",
            Self::EmptyString { .. } => "empty_string",
        }
    }

    /// The error envelope's `param`.
    #[must_use]
    pub fn param(&self) -> &str {
        match self {
            Self::InvalidId { param, .. } | Self::EmptyString { param } => param,
        }
    }
}

impl OutputMessage {
    /// Make a nonempty provider message ID valid for public history reuse.
    /// Empty-ID allocation remains owned by ingestion; upstream lookup keys
    /// are retained separately from the public response projection.
    pub(crate) fn normalize_public_id(id: &mut String) {
        if !id.is_empty() && !id.starts_with("msg_") {
            id.insert_str(0, "msg_");
        }
    }
}

impl ReasoningOutput {
    /// Make a nonempty provider reasoning ID valid for public history reuse, as for a message: `OpenAI` refuses a
    /// replayed reasoning item whose ID does not begin with `rs_`.
    pub(crate) fn normalize_public_id(id: &mut String) {
        if !id.is_empty() && !id.starts_with("rs_") {
            id.insert_str(0, "rs_");
        }
    }
}

impl FunctionToolCall {
    /// Normalize the public item identity without changing its execution call ID.
    /// Private calls lowered to another public tool shape keep their original identity.
    pub(crate) fn normalize_public_id(id: &mut String) {
        if !id.is_empty() && !id.starts_with("fc_") {
            id.insert_str(0, "fc_");
        }
    }
}

impl InputItem {
    /// Returns the public ID carried by this input item, when present.
    #[must_use]
    pub(crate) fn id(&self) -> Option<&str> {
        match self {
            Self::MultiAgentCall(item) => item.id.as_deref(),
            Self::MultiAgentCallOutput(item) => item.id.as_deref(),
            Self::AgentMessage(item) => item.id.as_deref(),
            Self::Message(item) => item.id.as_deref(),
            Self::FunctionCall(item) => item.id.as_deref(),
            Self::CodeInterpreterCall(item) => Some(&item.id),
            Self::ToolSearchCall(item) => Some(&item.id),
            Self::ToolSearchOutput(item) => item.id.as_deref(),
            // An unset ID decodes as empty: the item has none.
            Self::CustomToolCall(item) => Some(item.id.as_str()).filter(|id| !id.is_empty()),
            Self::ShellCall(item) => item.id.as_deref(),
            Self::ShellCallOutput(item) => item.id.as_deref(),
            Self::Reasoning(item) => Some(&item.id),
            Self::McpListTools(item) => Some(&item.id),
            Self::Compaction(item) => item.id.as_deref(),
            Self::WebSearchCall(item) => Some(&item.id),
            Self::McpCall(item) => Some(&item.id),
            Self::CustomToolCallOutput(item) => item.id.as_deref(),
            Self::FunctionCallOutput(item) => item.id.as_deref(),
            Self::CompactionTrigger => None,
        }
    }

    /// Returns the known public ID prefix, including its underscore separator.
    /// A missing rule does not impose a prefix on unverified or ID-less variants.
    #[must_use]
    pub(crate) fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::MultiAgentCall(_) => Some("mac_"),
            Self::MultiAgentCallOutput(_) => Some("maco_"),
            Self::AgentMessage(_) => Some("amsg_"),
            Self::Message(_) => Some("msg_"),
            Self::FunctionCall(_) | Self::FunctionCallOutput(_) => Some("fc_"),
            Self::CodeInterpreterCall(_) => Some("ci_"),
            Self::ToolSearchCall(_) => Some("tsc_"),
            Self::ToolSearchOutput(_) => Some("tso_"),
            Self::CustomToolCall(_) => Some("ctc_"),
            Self::ShellCall(_) => Some("sh_"),
            Self::Reasoning(_) => Some("rs_"),
            Self::McpListTools(_) => Some("mcpl_"),
            Self::Compaction(_) => Some("cmp_"),
            Self::WebSearchCall(_) => Some("ws_"),
            Self::McpCall(_) => Some("mcp_"),
            Self::ShellCallOutput(_) => Some("sho_"),
            Self::CustomToolCallOutput(_) => Some("ctco_"),
            Self::CompactionTrigger => None,
        }
    }
}

impl OutputItem {
    /// Returns the output item's wire ID, if the item has a known type.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::MultiAgentCall(item) => Some(&item.id),
            Self::MultiAgentCallOutput(item) => Some(&item.id),
            Self::AgentMessage(item) => Some(&item.id),
            Self::Message(item) => Some(&item.id),
            Self::FunctionCall(item) => Some(&item.id),
            Self::CodeInterpreterCall(item) => Some(&item.id),
            Self::ToolSearchCall(item) => Some(&item.id),
            Self::ToolSearchOutput(item) => item.id.as_deref(),
            // An unset ID decodes as empty: the item has none.
            Self::CustomToolCall(item) => Some(item.id.as_str()).filter(|id| !id.is_empty()),
            Self::ShellCall(item) => item.id.as_deref(),
            Self::WebSearchCall(item) => Some(&item.id),
            Self::McpCall(item) => Some(&item.id),
            Self::McpListTools(item) => Some(&item.id),
            Self::Reasoning(item) => Some(&item.id),
            Self::Compaction(item) => item.id.as_deref(),
            Self::Unknown => None,
        }
    }

    /// Returns the known public ID prefix, including its underscore separator.
    /// A missing rule does not impose a prefix on unverified or ID-less variants.
    #[must_use]
    pub(crate) fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::MultiAgentCall(_) => Some("mac_"),
            Self::MultiAgentCallOutput(_) => Some("maco_"),
            Self::AgentMessage(_) => Some("amsg_"),
            Self::Message(_) => Some("msg_"),
            Self::FunctionCall(_) => Some("fc_"),
            Self::CodeInterpreterCall(_) => Some("ci_"),
            Self::ToolSearchCall(_) => Some("tsc_"),
            Self::ToolSearchOutput(_) => Some("tso_"),
            Self::CustomToolCall(_) => Some("ctc_"),
            Self::ShellCall(_) => Some("sh_"),
            Self::WebSearchCall(_) => Some("ws_"),
            Self::McpCall(_) => Some("mcp_"),
            Self::McpListTools(_) => Some("mcpl_"),
            Self::Reasoning(_) => Some("rs_"),
            Self::Compaction(_) => Some("cmp_"),
            Self::Unknown => None,
        }
    }
}

impl InOutItem {
    /// Admits an item a client sent, as Responses input or as Conversations items: the one check every such item
    /// passes. As on `OpenAI`, its ID must begin with its type's prefix and a call or an output needs a non-empty
    /// `call_id` (repeated ones are accepted). `item` is its parameter path, such as `input[1]`.
    ///
    /// # Errors
    /// [`ClientItemError`] for a wrong ID prefix or an empty `call_id`.
    fn admit(&self, item: &str) -> Result<(), ClientItemError> {
        let (id, prefix) = match self {
            Self::Input(input) => (input.id(), input.id_prefix()),
            Self::Output(output) => (output.id(), output.id_prefix()),
        };
        if let (Some(id), Some(prefix)) = (id, prefix)
            && !id.starts_with(prefix)
        {
            return Err(ClientItemError::InvalidId {
                param: format!("{item}.id"),
                id: id.to_owned(),
                prefix: prefix.trim_end_matches('_'),
            });
        }
        if let Some(input) = self.as_input()
            && let Some(call_id) = input.call_id().or_else(|| input.output_call_id())
        {
            ClientCallId::validate(call_id).map_err(|_| ClientItemError::EmptyString {
                param: format!("{item}.call_id"),
            })?;
        }
        Ok(())
    }

    /// Admits the items a client sent in one request field (`input` or `items`), each named by its position.
    ///
    /// # Errors
    /// The first item's [`ClientItemError`].
    pub(crate) fn admit_all(items: &[Self], field: &str) -> Result<(), ClientItemError> {
        items
            .iter()
            .enumerate()
            .try_for_each(|(index, item)| item.admit(&format!("{field}[{index}]")))
    }

    /// Gives a client function call or tool output sent without a status the one `OpenAI` stores: a call is in
    /// progress and an output is completed.
    pub(crate) fn complete_omitted_status(&mut self) {
        match self {
            Self::Input(InputItem::FunctionCall(call)) => {
                call.status.get_or_insert(MessageStatus::InProgress);
            }
            Self::Input(InputItem::FunctionCallOutput(output)) => {
                output.status.get_or_insert(MessageStatus::Completed);
            }
            Self::Input(InputItem::CustomToolCallOutput(output)) => {
                output.status.get_or_insert(MessageStatus::Completed);
            }
            Self::Input(InputItem::ShellCallOutput(output)) => {
                output.status.get_or_insert(ShellCallStatus::Completed);
            }
            _ => {}
        }
    }

    /// The item's public identity, when supplied or assigned by storage.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Input(item) => item.id(),
            Self::Output(item) => item.id(),
        }
    }

    /// Assign the public resource identity without changing the item kind or payload.
    pub(crate) fn set_id(&mut self, id: String) {
        match self {
            Self::Input(item) => match item {
                InputItem::MultiAgentCall(item) => item.id = Some(id),
                InputItem::MultiAgentCallOutput(item) => item.id = Some(id),
                InputItem::AgentMessage(item) => item.id = Some(id),
                InputItem::Message(item) => item.id = Some(id),
                InputItem::FunctionCall(item) => item.id = Some(id),
                InputItem::ShellCall(item) => item.id = Some(id),
                InputItem::ShellCallOutput(item) => item.id = Some(id),
                InputItem::Compaction(item) => item.id = Some(id),
                InputItem::ToolSearchOutput(item) => item.id = Some(id),
                InputItem::CustomToolCallOutput(item) => item.id = Some(id),
                InputItem::CodeInterpreterCall(item) => item.id = id,
                InputItem::ToolSearchCall(item) => item.id = id,
                InputItem::CustomToolCall(item) => item.id = id,
                InputItem::Reasoning(item) => item.id = id,
                InputItem::McpListTools(item) => item.id = id,
                InputItem::WebSearchCall(item) => item.id = id,
                InputItem::McpCall(item) => item.id = id,
                InputItem::FunctionCallOutput(item) => item.id = Some(id),
                InputItem::CompactionTrigger => {}
            },
            Self::Output(item) => match item {
                OutputItem::MultiAgentCall(item) => item.id = id,
                OutputItem::MultiAgentCallOutput(item) => item.id = id,
                OutputItem::AgentMessage(item) => item.id = id,
                OutputItem::Message(item) => item.id = id,
                OutputItem::FunctionCall(item) => item.id = id,
                OutputItem::CodeInterpreterCall(item) => item.id = id,
                OutputItem::ToolSearchCall(item) => item.id = id,
                OutputItem::ToolSearchOutput(item) => item.id = Some(id),
                OutputItem::CustomToolCall(item) => item.id = id,
                OutputItem::WebSearchCall(item) => item.id = id,
                OutputItem::McpCall(item) => item.id = id,
                OutputItem::McpListTools(item) => item.id = id,
                OutputItem::Reasoning(item) => item.id = id,
                OutputItem::ShellCall(item) => item.id = Some(id),
                OutputItem::Compaction(item) => item.id = Some(id),
                OutputItem::Unknown => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::types::io::InOutItem;

    fn stored(value: Value) -> InOutItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn supplied_ids_are_validated_by_item_type_without_changing_call_ids() {
        let cases = [
            json!({"type":"message", "id":"msg_1", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"tool_search_call", "id":"tsc_1", "call_id":"call_1", "arguments":{}}),
            json!({"type":"function_call_output", "id":"fc_1", "call_id":"call_1", "output":"done"}),
            json!({"type":"custom_tool_call_output", "id":"ctco_1", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "id":"sho_1", "call_id":"call_1", "output":[]}),
            json!({"type":"tool_search_output", "id":"tso_1", "call_id":"call_1", "tools":[]}),
            json!({"type":"custom_tool_call", "id":"ctc_1", "call_id":"call_1", "name":"f", "input":"x"}),
            json!({"type":"shell_call", "id":"sh_1", "call_id":"call_1", "action":{"commands":["pwd"]}}),
            json!({"type":"reasoning", "id":"rs_1", "summary":[]}),
            json!({"type":"mcp_list_tools", "id":"mcpl_1", "server_label":"s", "tools":[]}),
            json!({"type":"mcp_call", "id":"mcp_1", "server_label":"s", "name":"f", "arguments":"{}"}),
            json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"test"}}),
            json!({"type":"compaction", "id":"cmp_1", "encrypted_content":"opaque"}),
        ];
        for mut value in cases {
            let item = stored(value.clone());
            item.admit("items[0]").expect("matching public ID prefix");
            value["id"] = json!("wrong_1");
            let item = stored(value);
            let error = item.admit("items[0]").unwrap_err();
            assert_eq!(error.code(), "invalid_value");
            assert_eq!(error.param(), "items[0].id");
        }
    }

    #[test]
    fn optional_ids_are_not_required() {
        for value in [
            json!({"type":"message", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "call_id":"call_1", "output":[]}),
        ] {
            stored(value).admit("items[0]").unwrap();
        }
    }

    #[test]
    fn invalid_id_reports_its_batch_index_and_rejects_empty_ids() {
        for id in ["", "item_wrongprefix"] {
            let items = [
                stored(json!({"type":"message", "role":"user", "content":"valid"})),
                stored(json!({"type":"message", "id":id, "role":"user", "content":"invalid"})),
            ];
            let error = InOutItem::admit_all(&items, "items").unwrap_err();
            assert_eq!(error.param(), "items[1].id");
            assert_eq!(
                error.to_string(),
                format!("Invalid 'items[1].id': '{id}'. Expected an ID that begins with 'msg'.")
            );
        }
    }

    /// Recorded `OpenAI` behavior: an empty `call_id` is refused on a call and on an output alike.
    #[test]
    fn empty_call_ids_are_refused_on_calls_and_outputs() {
        for value in [
            json!({"type": "function_call", "call_id": "", "name": "f", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "", "output": "done"}),
            json!({"type": "custom_tool_call", "call_id": "", "name": "f", "input": "x"}),
            json!({"type": "custom_tool_call_output", "call_id": "", "output": "done"}),
        ] {
            let error = stored(value).admit("input[1]").unwrap_err();
            assert_eq!(error.code(), "empty_string");
            assert_eq!(error.param(), "input[1].call_id");
            assert_eq!(
                error.to_string(),
                "Invalid 'input[1].call_id': empty string. Expected a string with minimum length 1, but got an empty \
                 string instead."
            );
        }
    }

    /// Recorded `OpenAI` behavior: a client function call stored without a status is in progress and an output is
    /// completed; a supplied status is kept.
    #[test]
    fn omitted_statuses_get_openai_defaults() {
        for (value, status) in [
            (
                json!({"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"}),
                "in_progress",
            ),
            (
                json!({"type": "function_call_output", "call_id": "call_1", "output": "done"}),
                "completed",
            ),
            (
                json!({"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}", "status": "completed"}),
                "completed",
            ),
        ] {
            let mut item = stored(value);
            item.complete_omitted_status();
            assert_eq!(serde_json::to_value(&item).unwrap()["status"], status);
        }
    }
}
