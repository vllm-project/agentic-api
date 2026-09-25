mod content;
mod conversions;

pub use content::{InputContent, InputFileContent, InputImageContent, InputTextContent, RefusalContent};

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::event::MessageStatus;
use crate::types::tools::{ResponsesTool, ToolSearchExecution, ToolSearchStatus};
use crate::utils::common::deserialize_from_value;

use super::multi_agent::{AgentAttribution, AgentMessage, MultiAgentCall, MultiAgentCallOutput};
use super::output::{CustomToolCall, FunctionToolCall, McpListTools, MessagePhase, ReasoningOutput, ToolSearchCall};
use super::shell::{ShellCall, ShellCallOutputMessage, ShellCallStatus};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    pub content: InputMessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputMessageContent {
    Text(String),
    Parts(Vec<InputContent>),
}

impl Default for InputMessageContent {
    fn default() -> Self {
        Self::Parts(Vec::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FunctionToolResultMessage {
    pub call_id: String,
    pub output: ToolCallOutput,
}

/// Text or structured content returned by a client-owned tool call.
///
/// The Responses API accepts either a string or an array containing text,
/// image, and file input content. Keeping the array structured preserves its
/// media semantics when a custom-tool output is normalized to a function-tool
/// output for the upstream model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolCallOutput {
    Text(String),
    Content(Vec<ToolOutputContent>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutputContent {
    InputText(InputTextContent),
    InputImage(InputImageContent),
    InputFile(InputFileContent),
}

#[cfg(feature = "openapi")]
mod openapi_schemas;

impl ToolCallOutput {
    #[must_use]
    pub fn has_content(&self) -> bool {
        match self {
            Self::Text(text) => !text.trim().is_empty(),
            Self::Content(content) => !content.is_empty(),
        }
    }
}

impl From<String> for ToolCallOutput {
    fn from(output: String) -> Self {
        Self::Text(output)
    }
}

impl From<&str> for ToolCallOutput {
    fn from(output: &str) -> Self {
        Self::Text(output.to_owned())
    }
}

/// A model-generated function call replayed as Responses input.
///
/// Input replay is intentionally more permissive than [`FunctionToolCall`]
/// output: clients may omit `id` and `status` when passing prior items to a
/// later request or to the compact endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputFunctionToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
}

impl From<FunctionToolCall> for InputFunctionToolCall {
    fn from(call: FunctionToolCall) -> Self {
        Self {
            agent: call.agent,
            id: Some(call.id),
            call_id: call.call_id,
            name: call.name,
            namespace: call.namespace,
            arguments: call.arguments,
            status: Some(call.status),
        }
    }
}

impl From<CustomToolCall> for InputFunctionToolCall {
    fn from(call: CustomToolCall) -> Self {
        Self {
            agent: call.agent,
            id: function_call_item_id(&call.id),
            call_id: call.call_id,
            name: call.name,
            namespace: None,
            arguments: serde_json::json!({ "input": call.input }).to_string(),
            status: call.status,
        }
    }
}

impl From<ShellCall> for InputFunctionToolCall {
    fn from(call: ShellCall) -> Self {
        Self {
            agent: call.agent,
            id: call.id.as_deref().and_then(function_call_item_id),
            call_id: call.call_id,
            name: "shell".to_owned(),
            namespace: None,
            // The action contains only JSON-compatible values and string map keys.
            arguments: serde_json::to_string(&call.action).expect("shell action serializes to JSON"),
            status: match call.status {
                Some(ShellCallStatus::Completed) => Some(MessageStatus::Completed),
                Some(ShellCallStatus::InProgress) => Some(MessageStatus::InProgress),
                Some(ShellCallStatus::Incomplete) | None => None,
            },
        }
    }
}

impl From<ShellCallOutputMessage> for FunctionToolResultMessage {
    fn from(output: ShellCallOutputMessage) -> Self {
        Self {
            call_id: output.call_id,
            // Command outputs contain only JSON-compatible values and string map keys.
            output: serde_json::to_string(&output.output)
                .expect("shell outputs serialize to JSON")
                .into(),
        }
    }
}

pub(super) fn deserialize_non_blank_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
        return Err(serde::de::Error::custom("value must not be blank"));
    }
    Ok(value)
}

/// A public model-generated tool-search call replayed as Responses input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputToolSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub call_id: String,
    #[serde(default)]
    pub execution: ToolSearchExecution,
    pub arguments: Value,
    #[serde(default)]
    pub status: ToolSearchStatus,
}

impl TryFrom<&ToolSearchCall> for InputToolSearchCall {
    type Error = ToolSearchStatus;

    fn try_from(call: &ToolSearchCall) -> Result<Self, Self::Error> {
        if call.status != ToolSearchStatus::Completed {
            return Err(call.status);
        }
        Ok(Self {
            agent: call.agent.clone(),
            id: call.id.clone(),
            call_id: call.call_id.clone(),
            execution: call.execution,
            arguments: call.arguments.clone(),
            status: ToolSearchStatus::Completed,
        })
    }
}

/// Client-returned declarations resolving a public tool-search call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ToolSearchOutputMessage {
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub call_id: String,
    #[serde(default)]
    pub execution: ToolSearchExecution,
    #[serde(default)]
    pub status: ToolSearchStatus,
    pub tools: Vec<ResponsesTool>,
}

/// An opaque compacted context checkpoint accepted as Responses input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompactionItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub encrypted_content: String,
}

/// Client result for a freeform custom tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CustomToolCallOutputMessage {
    pub call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub output: ToolCallOutput,
}

impl From<CustomToolCallOutputMessage> for FunctionToolResultMessage {
    fn from(output: CustomToolCallOutputMessage) -> Self {
        Self {
            call_id: output.call_id,
            output: output.output,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum InputItem {
    #[serde(rename = "multi_agent_call")]
    MultiAgentCall(MultiAgentCall),
    #[serde(rename = "multi_agent_call_output")]
    MultiAgentCallOutput(MultiAgentCallOutput),
    #[serde(rename = "agent_message")]
    AgentMessage(AgentMessage),
    #[serde(rename = "message")]
    Message(InputMessage),
    /// The model's tool invocation — appears in rehydrated history so vLLM sees
    /// the full call/output pair across turns.
    #[serde(rename = "function_call")]
    FunctionCall(InputFunctionToolCall),
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionToolResultMessage),
    #[serde(rename = "tool_search_call")]
    ToolSearchCall(InputToolSearchCall),
    #[serde(rename = "tool_search_output")]
    ToolSearchOutput(ToolSearchOutputMessage),
    /// The public freeform invocation accepted from a client request.
    #[serde(rename = "custom_tool_call")]
    CustomToolCall(CustomToolCall),
    #[serde(rename = "custom_tool_call_output")]
    CustomToolCallOutput(CustomToolCallOutputMessage),
    #[serde(rename = "shell_call")]
    ShellCall(ShellCall),
    #[serde(rename = "shell_call_output")]
    ShellCallOutput(ShellCallOutputMessage),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningOutput),
    /// Internal history record used by gateway orchestration to remember that
    /// an MCP server's tools were already listed. It is never sent to the model.
    #[serde(rename = "mcp_list_tools")]
    McpListTools(McpListTools),
    #[serde(rename = "compaction")]
    Compaction(CompactionItem),
    /// Codex CLI's remote-compaction V2 marker. Signals the server to run its
    /// own summarization turn and return exactly one `compaction` output item.
    /// Carries no payload; it is never forwarded to the upstream model.
    #[serde(rename = "compaction_trigger")]
    CompactionTrigger,
    #[serde(other)]
    Unknown,
}

impl<'de> Deserialize<'de> for InputItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut value = Value::deserialize(deserializer)?;
        // Consume the enum discriminator before a flattened payload can retain it.
        let kind = value.as_object_mut().and_then(|object| object.remove("type"));
        let item = match kind.as_ref().and_then(Value::as_str) {
            None | Some("message") => deserialize_from_value(value).map(Self::Message),
            Some("function_call") => deserialize_from_value(value).map(Self::FunctionCall),
            Some("function_call_output") => deserialize_from_value(value).map(Self::FunctionCallOutput),
            Some("tool_search_call") => deserialize_from_value(value).map(Self::ToolSearchCall),
            Some("tool_search_output") => deserialize_from_value(value).map(Self::ToolSearchOutput),
            Some("custom_tool_call") => deserialize_from_value(value).map(Self::CustomToolCall),
            Some("custom_tool_call_output") => deserialize_from_value(value).map(Self::CustomToolCallOutput),
            Some("shell_call") => deserialize_from_value(value).map(Self::ShellCall),
            Some("shell_call_output") => deserialize_from_value(value).map(Self::ShellCallOutput),
            Some("reasoning") => deserialize_from_value(value).map(Self::Reasoning),
            Some("mcp_list_tools") => deserialize_from_value(value).map(Self::McpListTools),
            Some("compaction") => deserialize_from_value(value).map(Self::Compaction),
            Some("multi_agent_call") => deserialize_from_value(value).map(Self::MultiAgentCall),
            Some("multi_agent_call_output") => deserialize_from_value(value).map(Self::MultiAgentCallOutput),
            Some("agent_message") => deserialize_from_value(value).map(Self::AgentMessage),
            Some("compaction_trigger") => Ok(Self::CompactionTrigger),
            Some(_) => return Ok(Self::Unknown),
        };
        item.map_err(serde::de::Error::custom)
    }
}

impl InputItem {
    #[must_use]
    pub(crate) fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    #[must_use]
    pub(crate) fn is_compaction_trigger(&self) -> bool {
        matches!(self, Self::CompactionTrigger)
    }

    #[must_use]
    pub(crate) fn is_model_visible(&self) -> bool {
        // Public collaboration data is opaque. The coordinator owns the separate
        // canonical plaintext context; never forward these items as model commands.
        !matches!(
            self,
            Self::McpListTools(_)
                | Self::CompactionTrigger
                | Self::MultiAgentCall(_)
                | Self::MultiAgentCallOutput(_)
                | Self::AgentMessage(_)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<InputItem>),
}

impl Default for ResponsesInput {
    /// An empty item list for programmatic request construction. This does not change
    /// whether the containing request requires an `input` field during deserialization.
    fn default() -> Self {
        Self::Items(Vec::new())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactionWindow {
    latest_index: usize,
    retained_start: usize,
}

impl CompactionWindow {
    #[must_use]
    pub(crate) const fn latest_index(self) -> usize {
        self.latest_index
    }

    #[must_use]
    pub(crate) fn retains_user_item(self, index: usize, item: &InputItem) -> bool {
        index >= self.retained_start
            && index < self.latest_index
            && matches!(item, InputItem::Message(message)
                if message.role == "user"
                    && message.id.is_some()
                    && message.status == Some(MessageStatus::Completed))
    }
}

#[must_use]
pub(crate) fn latest_compaction_window(items: &[InputItem]) -> Option<CompactionWindow> {
    let latest_index = items
        .iter()
        .rposition(|item| matches!(item, InputItem::Compaction(_)))?;
    let retained_start = items[..latest_index]
        .iter()
        .rposition(|item| matches!(item, InputItem::Compaction(_)))
        .map_or(0, |index| index + 1);
    Some(CompactionWindow {
        latest_index,
        retained_start,
    })
}

pub(crate) fn model_items(items: &[InputItem]) -> impl Iterator<Item = &InputItem> {
    let window = latest_compaction_window(items);
    items
        .iter()
        .enumerate()
        .filter(move |(index, item)| {
            item.is_model_visible()
                && window.is_none_or(|window| *index >= window.latest_index() || window.retains_user_item(*index, item))
        })
        .map(|(_, item)| item)
}

impl ResponsesInput {
    /// Iterate over the items in the canonical context sent to vLLM without cloning them.
    pub(crate) fn model_items(&self) -> impl Iterator<Item = &InputItem> {
        let items = match self {
            Self::Text(_) => &[][..],
            Self::Items(items) => items.as_slice(),
        };
        model_items(items)
    }

    #[must_use]
    pub fn contains_compaction(&self) -> bool {
        matches!(self, Self::Items(items) if items.iter().any(|item| matches!(item, InputItem::Compaction(_))))
    }

    #[must_use]
    pub fn has_compaction_trigger(&self) -> bool {
        matches!(self, Self::Items(items) if items.iter().any(InputItem::is_compaction_trigger))
    }

    /// Return the canonical context sent to vLLM.
    ///
    /// vLLM does not understand public `compaction` items, so the latest item
    /// becomes an assistant message containing the locally generated summary.
    /// Items before that checkpoint are superseded and are omitted.
    /// Internal MCP-list records and `compaction_trigger` markers are stripped
    /// and never reach the model.
    #[must_use]
    pub fn model_input(&self) -> Cow<'_, Self> {
        let Self::Items(items) = self else {
            return Cow::Borrowed(self);
        };

        if latest_compaction_window(items).is_none() {
            if items.iter().any(|item| !item.is_model_visible()) {
                let stripped = self.model_items().cloned().collect();
                return Cow::Owned(Self::Items(stripped));
            }
            return Cow::Borrowed(self);
        }

        let model_items = self
            .model_items()
            .map(|item| match item {
                InputItem::Compaction(compaction) => InputItem::Message(InputMessage {
                    role: "assistant".to_owned(),
                    content: InputMessageContent::Parts(vec![InputContent::OutputText(InputTextContent::new(
                        compaction.encrypted_content.clone(),
                    ))]),
                    ..Default::default()
                }),
                other => other.clone(),
            })
            .collect();
        Cow::Owned(Self::Items(model_items))
    }
}

fn function_call_item_id(item_id: &str) -> Option<String> {
    if item_id.is_empty() {
        return None;
    }
    if let Some(suffix) = item_id
        .strip_prefix("ctc_")
        .or_else(|| item_id.strip_prefix("sh_"))
        .filter(|suffix| !suffix.is_empty())
    {
        return Some(format!("fc_{suffix}"));
    }
    Some(item_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_phase_survives_input_parsing_and_model_projection() {
        for (phase, expected) in [
            (None, None),
            (Some("commentary"), Some(MessagePhase::Commentary)),
            (Some("final_answer"), Some(MessagePhase::FinalAnswer)),
        ] {
            let mut wire = serde_json::json!({
                "type": "message",
                "id": "msg_history",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "Review result."}]
            });
            if let Some(phase) = phase {
                wire["phase"] = serde_json::json!(phase);
            }
            let item: InputItem = serde_json::from_value(wire.clone()).unwrap();
            let InputItem::Message(message) = &item else {
                panic!("expected a message item");
            };
            assert_eq!(message.phase, expected);
            let input = ResponsesInput::Items(vec![item]);
            assert_eq!(
                serde_json::to_value(input.model_input()).unwrap(),
                serde_json::json!([wire])
            );
        }
    }

    #[test]
    fn structured_input_without_message_type() {
        // The full request body from vllm-project/agentic-api#150. `ResponsesInput`
        // models the `input` field's value, so pull that field out before
        // deserializing, mirroring how the request struct's field is populated.
        let body: Value = serde_json::from_str(
            r#"{
                "model": "dummy",
                "input": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": "hi new"
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("issue payload is valid json");

        let input: ResponsesInput =
            serde_json::from_value(body["input"].clone()).expect("structured input without message type parses");

        let ResponsesInput::Items(items) = input else {
            panic!("expected ResponsesInput::Items");
        };
        assert_eq!(items.len(), 1);

        let InputItem::Message(message) = &items[0] else {
            panic!("expected InputItem::Message");
        };
        assert_eq!(message.role, "user");

        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("expected structured content parts");
        };
        assert_eq!(parts.len(), 1);
        let InputContent::InputText(text) = &parts[0] else {
            panic!("expected InputContent::InputText");
        };
        assert_eq!(text.text, "hi new");
    }

    #[test]
    fn issue_150_shorthand_message_mixes_with_typed_items() {
        // A shorthand message (no `"type"` tag) alongside explicitly typed
        // history items must all deserialize into their own `InputItem` variant.
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hi new"}
                ]
            },
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{}"
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": "done"
            }
        ]))
        .expect("shorthand message mixed with typed items parses");

        let ResponsesInput::Items(items) = input else {
            panic!("expected ResponsesInput::Items");
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], InputItem::Message(message) if message.role == "user"));
        assert!(matches!(&items[1], InputItem::FunctionCall(call) if call.name == "lookup"));
        assert!(matches!(&items[2], InputItem::CustomToolCallOutput(output) if output.call_id == "call_1"));
    }

    #[test]
    fn issue_150_shorthand_message_with_mixed_content_parts() {
        // Shorthand messages should support the same structured content
        // vocabulary as explicitly typed ones, including multiple part types.
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "look at this"},
                {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "low"}
            ]
        }]))
        .expect("shorthand message with mixed content parts parses");

        let ResponsesInput::Items(items) = input else {
            panic!("expected ResponsesInput::Items");
        };
        assert_eq!(items.len(), 1);

        let InputItem::Message(message) = &items[0] else {
            panic!("expected InputItem::Message");
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("expected structured content parts");
        };
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], InputContent::InputText(text) if text.text == "look at this"));
        assert!(matches!(&parts[1], InputContent::InputImage(image) if image.detail.as_deref() == Some("low")));
    }

    #[test]
    fn tool_search_replay_defaults_are_canonicalized() {
        let call: InputItem = serde_json::from_value(serde_json::json!({
            "type": "tool_search_call",
            "id": "tsc_1",
            "call_id": "call_search_1",
            "arguments": ["weather", "timezone"]
        }))
        .expect("valid replayed search call");
        let output: InputItem = serde_json::from_value(serde_json::json!({
            "type": "tool_search_output",
            "call_id": "call_search_1",
            "tools": []
        }))
        .expect("valid empty search result");

        assert_eq!(
            serde_json::to_value(call).expect("call serializes"),
            serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "execution": "client",
                "arguments": ["weather", "timezone"],
                "status": "completed"
            })
        );
        assert_eq!(
            serde_json::to_value(output).expect("output serializes"),
            serde_json::json!({
                "type": "tool_search_output",
                "call_id": "call_search_1",
                "execution": "client",
                "status": "completed",
                "tools": []
            })
        );
    }

    #[test]
    fn tool_search_items_accept_documented_statuses() {
        for status in ["in_progress", "completed", "incomplete"] {
            let call: InputItem = serde_json::from_value(serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "arguments": {"query": "weather"},
                "status": status
            }))
            .expect("documented tool-search call status");
            let output: InputItem = serde_json::from_value(serde_json::json!({
                "type": "tool_search_output",
                "call_id": "call_search_1",
                "status": status,
                "tools": []
            }))
            .expect("documented tool-search output status");

            assert_eq!(serde_json::to_value(call).expect("call serializes")["status"], status);
            assert_eq!(
                serde_json::to_value(output).expect("output serializes")["status"],
                status
            );
        }
    }

    #[test]
    fn tool_search_replay_rejects_invalid_known_shapes() {
        for item in [
            serde_json::json!({
                "type": "tool_search_call",
                "call_id": "call_search_1",
                "arguments": {"query": "missing required item id"}
            }),
            serde_json::json!({
                "type": "tool_search_call",
                "id": "   ",
                "call_id": "call_search_1",
                "arguments": {"query": "blank item id"}
            }),
            serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "   ",
                "arguments": {"query": "blank call id"}
            }),
            serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "execution": "server",
                "arguments": {"query": "unsupported execution"}
            }),
            serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "status": "completed"
            }),
            serde_json::json!({
                "type": "tool_search_output",
                "call_id": "call_search_1"
            }),
        ] {
            assert!(
                serde_json::from_value::<InputItem>(item).is_err(),
                "malformed known tool-search item must not become Unknown"
            );
        }

        let future: InputItem = serde_json::from_value(serde_json::json!({
            "type": "future_search_item",
            "payload": {"opaque": true}
        }))
        .expect("unrelated future item remains forward-compatible");
        assert!(matches!(future, InputItem::Unknown));
    }

    #[test]
    fn function_call_input_accepts_missing_status() {
        let item: InputItem = serde_json::from_value(serde_json::json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "lookup",
            "arguments": "{}"
        }))
        .expect("valid replay input");

        let InputItem::FunctionCall(call) = item else {
            panic!("expected function call");
        };
        assert_eq!(call.status, None);
    }

    #[test]
    fn malformed_known_type_is_not_reinterpreted_as_shorthand_message() {
        let result = serde_json::from_value::<InputItem>(serde_json::json!({
            "type": "function_call",
            "role": "user",
            "content": "not a function call"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn structured_custom_tool_output_is_preserved_when_normalized() {
        let content = serde_json::json!([
            {"type": "input_text", "text": "diagram"},
            {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "low"},
            {"type": "input_file", "file_id": "file_123", "filename": "report.pdf"}
        ]);
        let item: InputItem = serde_json::from_value(serde_json::json!({
            "type": "custom_tool_call_output",
            "call_id": "call_1",
            "output": content
        }))
        .expect("valid structured custom-tool output");

        let InputItem::CustomToolCallOutput(output) = item else {
            panic!("expected custom-tool output");
        };
        let normalized = FunctionToolResultMessage::from(output);
        let value = serde_json::to_value(normalized).expect("normalized output serializes");

        assert_eq!(value["output"], content);
    }

    #[test]
    fn structured_function_tool_output_preserves_image_array() {
        let content = serde_json::json!([
            {"type": "input_text", "text": "attached local image path: diagram.png"},
            {"type": "input_image", "image_url": "data:image/png;base64,abc"}
        ]);
        let item: InputItem = serde_json::from_value(serde_json::json!({
            "type": "function_call_output",
            "call_id": "call_view_image_1",
            "output": content
        }))
        .expect("valid structured function-tool output");

        let InputItem::FunctionCallOutput(output) = &item else {
            panic!("expected function-tool output");
        };
        assert!(matches!(output.output, ToolCallOutput::Content(_)));

        let value = serde_json::to_value(&item).expect("output serializes");
        assert_eq!(value["output"], content, "structured output must not be stringified");
    }

    #[test]
    fn unmodeled_message_content_part_keeps_its_type_name() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "before"},
                {"type": "input_audio", "audio_url": "https://example.com/clip.wav"},
                {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "low"}
            ]
        }]))
        .expect("an unmodeled part must not fail deserialization");

        let ResponsesInput::Items(items) = &input else {
            panic!("expected items");
        };
        let InputItem::Message(message) = &items[0] else {
            panic!("expected message item");
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("expected message parts");
        };
        assert!(
            matches!(parts.as_slice(), [InputContent::InputText(_), InputContent::Unknown(kind), InputContent::InputImage(_)]
                if kind == "input_audio"),
            "the unmodeled part must keep its position and its type name"
        );
        assert!(
            serde_json::to_value(&input).is_err(),
            "an unmodeled part must never serialize into a synthetic part"
        );
    }

    #[test]
    fn extension_fields_on_modeled_parts_round_trip() {
        // The typed path must forward a known part exactly as the client sent
        // it, unmodeled fields included, like the raw proxy path does.
        let parts = serde_json::json!([
            {"type": "input_text", "text": "look", "x_future_text": true},
            {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "low", "x_future_field": "kept"},
            {"type": "output_text", "text": "seen", "annotations": [], "logprobs": []},
            {"type": "refusal", "refusal": "no", "x_reason": "policy"}
        ]);
        let message: InputMessage = serde_json::from_value(serde_json::json!({"role": "user", "content": parts}))
            .expect("modeled parts with extension fields deserialize");
        assert_eq!(
            serde_json::to_value(&message).expect("message serializes")["content"],
            parts,
            "extension fields must survive the typed round trip"
        );

        let output: ToolCallOutput = serde_json::from_value(serde_json::json!([
            {"type": "input_image", "image_url": "data:image/png;base64,abc", "x_future_field": "kept"}
        ]))
        .expect("structured tool output deserializes");
        assert_eq!(
            serde_json::to_value(&output).expect("output serializes")[0]["x_future_field"],
            "kept",
            "tool-output image parts keep extension fields too"
        );
    }

    #[test]
    fn message_content_part_without_a_type_is_rejected() {
        let error = serde_json::from_value::<InputContent>(serde_json::json!({"text": "no type"}))
            .expect_err("a part without a type has no wire meaning");
        assert!(error.to_string().contains("missing a string `type`"), "{error}");
    }

    #[test]
    fn refusal_content_round_trips_in_assistant_history() {
        let part = serde_json::json!({"type": "refusal", "refusal": "I can't help with that."});
        let content: InputContent = serde_json::from_value(part.clone()).expect("refusal is a modeled part");
        assert!(matches!(&content, InputContent::Refusal(refusal) if refusal.refusal == "I can't help with that."));
        assert_eq!(serde_json::to_value(&content).expect("refusal serializes"), part);
    }

    #[test]
    fn custom_tool_output_rejects_unsupported_shapes() {
        for output in [
            serde_json::json!({"result": "not a supported top-level object"}),
            serde_json::json!([{"type": "output_text", "text": "wrong content type"}]),
            serde_json::json!(["content items must be objects"]),
        ] {
            let result = serde_json::from_value::<InputItem>(serde_json::json!({
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": output
            }));

            assert!(result.is_err(), "unsupported custom-tool output should fail");
        }
    }

    #[test]
    fn compaction_item_becomes_assistant_model_context() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([{
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "summary"
        }]))
        .expect("valid compaction input");

        let model_input = input.model_input();
        let serialized = serde_json::to_value(model_input).expect("model input serializes");
        assert_eq!(serialized[0]["role"], "assistant");
        assert_eq!(serialized[0]["content"][0]["type"], "output_text");
        assert_eq!(serialized[0]["content"][0]["text"], "summary");
    }

    #[test]
    fn compaction_trigger_parses_as_dedicated_variant() {
        let item: InputItem = serde_json::from_value(serde_json::json!({"type": "compaction_trigger"}))
            .expect("compaction_trigger parses");
        assert!(item.is_compaction_trigger());

        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"role": "user", "content": "history"},
            {"type": "compaction_trigger"}
        ]))
        .expect("trigger input parses");
        assert!(input.has_compaction_trigger());
        assert!(!input.contains_compaction());
    }

    #[test]
    fn model_input_strips_compaction_trigger_without_window() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"role": "user", "content": "history"},
            {"type": "compaction_trigger"}
        ]))
        .expect("trigger input parses");

        let serialized = serde_json::to_value(input.model_input()).expect("model input serializes");
        assert_eq!(serialized.as_array().map(Vec::len), Some(1));
        assert_eq!(serialized[0]["type"], "message");
        assert_eq!(serialized[0]["content"], "history");
    }

    #[test]
    fn model_input_strips_internal_mcp_list_tools() {
        let input = ResponsesInput::Items(vec![
            InputItem::McpListTools(McpListTools::new("mcpl_1", "counter", Vec::new())),
            InputItem::Message(InputMessage {
                role: "user".to_owned(),
                content: InputMessageContent::Text("continue".to_owned()),
                ..Default::default()
            }),
        ]);

        let serialized = serde_json::to_value(input.model_input()).expect("model input serializes");
        assert_eq!(serialized.as_array().map(Vec::len), Some(1));
        assert_eq!(serialized[0]["content"], "continue");
        assert!(
            serialized
                .as_array()
                .is_some_and(|items| { items.iter().all(|item| item["type"] != "mcp_list_tools") })
        );
    }

    #[test]
    fn model_input_strips_compaction_trigger_after_window() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"role": "user", "content": "discard me"},
            {"type": "compaction", "encrypted_content": "summary"},
            {"type": "message", "id": "msg_keep", "role": "user", "status": "completed", "content": "retained"},
            {"type": "compaction_trigger"}
        ]))
        .expect("trigger input parses");

        let serialized = serde_json::to_value(input.model_input()).expect("model input serializes");
        assert_eq!(serialized.as_array().map(Vec::len), Some(2));
        assert_eq!(serialized[0]["role"], "assistant");
        assert_eq!(serialized[0]["content"][0]["text"], "summary");
        assert_eq!(serialized[1]["content"], "retained");
        assert!(
            serialized
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item["type"] != "compaction_trigger"))
        );
    }

    #[test]
    fn latest_compaction_preserves_canonical_user_messages_and_supersedes_prior_context() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"role": "user", "content": "discard me"},
            {"type": "compaction", "encrypted_content": "old summary"},
            {"role": "assistant", "content": "also discard me"},
            {"type": "message", "id": "msg_keep", "role": "user", "status": "completed", "content": "retained user"},
            {"type": "compaction", "encrypted_content": "latest summary"},
            {"role": "user", "content": "keep me"}
        ]))
        .expect("valid compacted history");

        let serialized = serde_json::to_value(input.model_input()).expect("model input serializes");
        assert_eq!(serialized.as_array().map(Vec::len), Some(3));
        assert_eq!(serialized[0]["content"], "retained user");
        assert_eq!(serialized[1]["role"], "assistant");
        assert_eq!(serialized[1]["content"][0]["text"], "latest summary");
        assert_eq!(serialized[2]["content"], "keep me");
    }

    #[test]
    fn custom_items_convert_to_function_history() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {
                "type": "custom_tool_call",
                "id": "ctc_1",
                "call_id": "call_1",
                "name": "raw_echo",
                "input": "hello",
                "status": "completed"
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": "done"
            }
        ]))
        .expect("custom history");

        let canonical_value = serde_json::to_value(Vec::<InputItem>::from(&input)).expect("canonical items");
        assert_eq!(canonical_value[0]["type"], "function_call");
        assert_eq!(canonical_value[0]["id"], "fc_1");
        assert_eq!(canonical_value[0]["arguments"], r#"{"input":"hello"}"#);
        assert_eq!(canonical_value[1]["type"], "function_call_output");
        assert_eq!(canonical_value[1]["output"], "done");

        let public_value = serde_json::to_value(input).expect("public input");
        assert_eq!(public_value[0]["type"], "custom_tool_call");
        assert_eq!(public_value[1]["type"], "custom_tool_call_output");
    }

    #[test]
    fn shell_call_and_output_parse_as_typed_input_items() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {
                "type": "shell_call",
                "id": "sh_1",
                "call_id": "call_1",
                "action": {"commands": ["pwd"], "timeout_ms": 1000},
                "status": "completed"
            },
            {
                "type": "shell_call_output",
                "call_id": "call_1",
                "max_output_length": 4096,
                "output": [{
                    "stdout": "/workspace\n",
                    "stderr": "",
                    "outcome": {"type": "exit", "exit_code": 0}
                }],
                "status": "completed"
            }
        ]))
        .expect("shell history");

        let ResponsesInput::Items(items) = &input else {
            panic!("expected item input");
        };
        assert!(matches!(items[0], InputItem::ShellCall(_)));
        assert!(matches!(items[1], InputItem::ShellCallOutput(_)));

        let borrowed = serde_json::to_value(Vec::<InputItem>::from(&input)).unwrap();
        let owned = serde_json::to_value(Vec::<InputItem>::from(input.clone())).unwrap();
        let prepared = ResponsesInput::Items(Vec::from(&input));
        let model = serde_json::to_value(prepared.model_input()).unwrap();
        assert_eq!(borrowed, owned);
        assert_eq!(borrowed, model);
        assert_eq!(model[0]["type"], "function_call");
        assert_eq!(model[0]["id"], "fc_1");
        assert_eq!(model[0]["name"], "shell");
        assert_eq!(model[0]["call_id"], model[1]["call_id"]);
        assert_eq!(model[1]["type"], "function_call_output");
        let action: Value = serde_json::from_str(model[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(action, serde_json::json!({"commands": ["pwd"], "timeout_ms": 1000}));
        let output: Value = serde_json::from_str(model[1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output[0]["outcome"]["exit_code"], 0);

        let serialized = serde_json::to_value(input).expect("shell history serializes");
        assert_eq!(serialized[0]["type"], "shell_call");
        assert_eq!(serialized[1]["type"], "shell_call_output");
        assert_eq!(serialized[1]["output"][0]["outcome"]["exit_code"], 0);
    }
}
