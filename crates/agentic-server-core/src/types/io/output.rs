mod apply_done;
pub use apply_done::ApplyDone;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::events::EventPayload;
use crate::events::types::ShellCommandUpdate;
use crate::executor::error::ExecutorError;
use crate::tool::ToolRegistry;
use crate::types::event::MessageStatus;
use crate::types::tools::{ToolSearchExecution, ToolSearchStatus};
use crate::utils::common::deserialize_from_value_opt;
use crate::utils::uuid7_str;

use super::input::{
    CompactionItem, InputContent, InputFunctionToolCall, InputItem, InputMessage, InputMessageContent,
    InputTextContent, InputToolSearchCall, deserialize_non_blank_string,
};
use super::multi_agent::{AgentAttribution, AgentMessage, MultiAgentCall, MultiAgentCallOutput};
use super::shell::ShellCall;

/// Text emitted by an assistant or a hosted collaboration action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputTextContent {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<OutputTextLogprob>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TopLogprob {
    pub token: String,
    pub bytes: Vec<u8>,
    pub logprob: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputTextLogprob {
    pub token: String,
    pub bytes: Vec<u8>,
    pub logprob: f64,
    pub top_logprobs: Vec<TopLogprob>,
}

impl OutputTextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            annotations: Some(vec![]),
            logprobs: None,
        }
    }
}

/// Public messages include assistant output and forked user input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMessageContent {
    InputText(InputTextContent),
    OutputText(OutputTextContent),
}

impl OutputMessageContent {
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::InputText(part) => &part.text,
            Self::OutputText(part) => &part.text,
        }
    }
}

impl From<OutputTextContent> for OutputMessageContent {
    fn from(part: OutputTextContent) -> Self {
        Self::OutputText(part)
    }
}

impl From<OutputMessageContent> for InputContent {
    fn from(part: OutputMessageContent) -> Self {
        match part {
            OutputMessageContent::InputText(part) => Self::InputText(part),
            OutputMessageContent::OutputText(part) => {
                let mut text = InputTextContent::new(part.text);
                if let Some(annotations) = part.annotations {
                    text.extra.insert("annotations".into(), Value::Array(annotations));
                }
                if let Some(logprobs) = part.logprobs {
                    // These fixed structs contain only JSON-compatible values and string keys.
                    text.extra.insert(
                        "logprobs".into(),
                        serde_json::to_value(logprobs).expect("text log probabilities serialize to JSON"),
                    );
                }
                Self::OutputText(text)
            }
        }
    }
}

/// The purpose of an assistant message, independent of its lifecycle status.
/// A completed commentary message is not a final answer, and a final answer from
/// one agent does not by itself complete the public response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    pub id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
    pub status: MessageStatus,
    #[serde(default)]
    pub content: Vec<OutputMessageContent>,
}

impl OutputMessage {
    pub fn new(id: impl Into<String>, status: MessageStatus) -> Self {
        Self {
            agent: None,
            id: id.into(),
            role: "assistant".into(),
            phase: None,
            status,
            content: vec![],
        }
    }
}

impl TryFrom<&EventPayload> for OutputMessage {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded {
            item_id, initial_item, ..
        } = payload
        else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        if let Some(OutputItem::Message(item)) = initial_item.as_deref() {
            return Ok(item.clone());
        }
        let id = if item_id.is_empty() {
            uuid7_str("msg_")
        } else {
            item_id.clone()
        };
        Ok(Self::new(id, MessageStatus::InProgress))
    }
}

impl From<OutputMessage> for InputMessage {
    fn from(msg: OutputMessage) -> Self {
        let parts = msg.content.into_iter().map(InputContent::from).collect();
        Self {
            id: Some(msg.id),
            role: msg.role,
            phase: msg.phase,
            agent: msg.agent,
            status: Some(msg.status),
            content: InputMessageContent::Parts(parts),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FunctionToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default = "default_function_call_id")]
    #[serde(deserialize_with = "deserialize_function_call_id")]
    pub id: String,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default)]
    pub arguments: String,
    #[serde(default = "default_completed_status")]
    #[serde(deserialize_with = "deserialize_status_or_default")]
    pub status: MessageStatus,
}

/// A newly emitted public client tool-search call.
///
/// Unlike replay input, execution and status have no serde defaults: response
/// translation must populate both fields explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ToolSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub call_id: String,
    pub execution: ToolSearchExecution,
    pub arguments: Value,
    pub status: ToolSearchStatus,
}

/// A freeform custom tool invocation.
///
/// `input` is opaque text and must not be parsed as function-call JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CustomToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: String,
}

fn default_completed_status() -> MessageStatus {
    MessageStatus::Completed
}

fn default_function_call_id() -> String {
    uuid7_str("fc_")
}

fn deserialize_function_call_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?
        .filter(|id| !id.is_empty())
        .unwrap_or_else(default_function_call_id))
}

fn deserialize_status_or_default<'de, D>(deserializer: D) -> Result<MessageStatus, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<MessageStatus> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or(MessageStatus::Completed))
}

impl TryFrom<&EventPayload> for FunctionToolCall {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded {
            item_id,
            call_id,
            name,
            namespace,
            initial_item,
            ..
        } = payload
        else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("fc_")
        } else {
            item_id.clone()
        };
        Ok(Self {
            agent: match initial_item.as_deref() {
                Some(OutputItem::FunctionCall(call)) => call.agent.clone(),
                _ => None,
            },
            id,
            call_id: call_id.as_deref().unwrap_or_default().to_owned(),
            name: name.as_deref().unwrap_or_default().to_owned(),
            namespace: namespace.clone(),
            arguments: String::new(),
            status: MessageStatus::InProgress,
        })
    }
}

impl TryFrom<&EventPayload> for ToolSearchCall {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, call_id, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let call_id = call_id
            .as_deref()
            .filter(|call_id| !call_id.trim().is_empty())
            .ok_or_else(|| ExecutorError::ParseError("tool_search_call is missing call_id".into()))?;
        if item_id.trim().is_empty() {
            return Err(ExecutorError::ParseError("tool_search_call is missing id".into()));
        }
        Ok(Self {
            agent: None,
            id: item_id.clone(),
            call_id: call_id.to_owned(),
            execution: ToolSearchExecution::Client,
            arguments: Value::Object(serde_json::Map::new()),
            status: ToolSearchStatus::InProgress,
        })
    }
}

impl TryFrom<&EventPayload> for CustomToolCall {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded {
            item_id, call_id, name, ..
        } = payload
        else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("ctc_")
        } else {
            item_id.clone()
        };
        Ok(Self {
            agent: None,
            id,
            status: Some(MessageStatus::InProgress),
            call_id: call_id.as_deref().unwrap_or_default().to_owned(),
            name: name.as_deref().unwrap_or_default().to_owned(),
            input: String::new(),
        })
    }
}

impl TryFrom<&EventPayload> for CompactionItem {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("cmp_")
        } else {
            item_id.clone()
        };
        Ok(Self {
            agent: None,
            id: Some(id),
            encrypted_content: String::new(),
        })
    }
}

impl TryFrom<&EventPayload> for ShellCall {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded {
            initial_item: Some(item),
            ..
        } = payload
        else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let OutputItem::ShellCall(call) = item.as_ref() else {
            return Err(ExecutorError::ParseError("expected initial shell item".into()));
        };
        Ok(call.clone())
    }
}

impl ApplyDone for ShellCall {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String) {
        match payload {
            EventPayload::ShellCallCommand {
                command_index,
                update: ShellCommandUpdate::Done(command),
                ..
            } => {
                if let Some(target) = self.action.commands.get_mut(*command_index as usize) {
                    *target = if command.is_empty() {
                        std::mem::take(buffer)
                    } else {
                        buffer.clear();
                        command.clone()
                    };
                }
            }
            EventPayload::OutputItemDone { item, .. } => {
                // Deserialize through the tagged enum so `type` cannot enter flattened extras.
                if let Some(OutputItem::ShellCall(call)) = deserialize_from_value_opt(item.clone()) {
                    *self = call;
                    buffer.clear();
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum GatewayCallStatus {
    InProgress,
    Completed,
    Failed,
}

impl GatewayCallStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

pub type WebSearchCallStatus = GatewayCallStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpCallStatus {
    InProgress,
    Calling,
    Completed,
    Incomplete,
    Failed,
}

impl From<GatewayCallStatus> for McpCallStatus {
    fn from(status: GatewayCallStatus) -> Self {
        match status {
            GatewayCallStatus::InProgress => Self::InProgress,
            GatewayCallStatus::Completed => Self::Completed,
            GatewayCallStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WebSearchSource {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WebSearchActionSearch {
    #[serde(skip, default = "default_web_search_action_search_type")]
    pub type_: String,
    pub query: String,
    #[serde(default)]
    pub queries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<WebSearchSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WebSearchActionError {
    #[error("web search action requires at least one query")]
    EmptyQueries,
}

fn default_web_search_action_search_type() -> String {
    "search".to_owned()
}

impl WebSearchActionSearch {
    /// Builds a search action from a non-empty query list.
    ///
    /// # Errors
    ///
    /// Returns [`WebSearchActionError::EmptyQueries`] if `queries` is empty.
    pub fn try_new(queries: Vec<String>, sources: Vec<WebSearchSource>) -> Result<Self, WebSearchActionError> {
        let query = queries.first().cloned().ok_or(WebSearchActionError::EmptyQueries)?;
        Ok(Self {
            type_: default_web_search_action_search_type(),
            query,
            queries,
            sources,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WebSearchActionOpenPage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WebSearchActionFindInPage {
    pub pattern: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WebSearchAction {
    Search(WebSearchActionSearch),
    OpenPage(WebSearchActionOpenPage),
    FindInPage(WebSearchActionFindInPage),
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for WebSearchAction {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::Ref;
        use utoipa::openapi::schema::{AllOfBuilder, ObjectBuilder, OneOfBuilder, SchemaType, Type};

        fn tagged(type_value: &str, schema: &str) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
            AllOfBuilder::new()
                .item(
                    ObjectBuilder::new()
                        .property(
                            "type",
                            ObjectBuilder::new()
                                .schema_type(SchemaType::new(Type::String))
                                .enum_values(Some([type_value])),
                        )
                        .required("type"),
                )
                .item(Ref::from_schema_name(schema))
                .into()
        }

        OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(tagged("search", "WebSearchActionSearch"))
            .item(tagged("open_page", "WebSearchActionOpenPage"))
            .item(tagged("find_in_page", "WebSearchActionFindInPage"))
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for WebSearchAction {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("WebSearchAction")
    }
}

impl WebSearchAction {
    #[must_use]
    pub const fn type_str(&self) -> &'static str {
        match self {
            Self::Search(_) => "search",
            Self::OpenPage(_) => "open_page",
            Self::FindInPage(_) => "find_in_page",
        }
    }

    #[must_use]
    pub const fn as_search(&self) -> Option<&WebSearchActionSearch> {
        match self {
            Self::Search(action) => Some(action),
            Self::OpenPage(_) | Self::FindInPage(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WebSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    pub id: String,
    pub status: WebSearchCallStatus,
    pub action: WebSearchAction,
}

impl WebSearchCall {
    /// Builds a web-search call from a non-empty query list.
    ///
    /// # Errors
    ///
    /// Returns [`WebSearchActionError::EmptyQueries`] if `queries` is empty.
    pub fn try_new(
        id: impl Into<String>,
        status: WebSearchCallStatus,
        queries: Vec<String>,
        sources: Vec<WebSearchSource>,
    ) -> Result<Self, WebSearchActionError> {
        Ok(Self {
            agent: None,
            id: id.into(),
            status,
            action: WebSearchAction::Search(WebSearchActionSearch::try_new(queries, sources)?),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpCallError {
    Text(String),
    ToolExecution(McpToolExecutionError),
    Unknown(Value),
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for McpCallError {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::AnyOfBuilder::new()
            .item(
                utoipa::openapi::ObjectBuilder::new().schema_type(utoipa::openapi::schema::SchemaType::new(
                    utoipa::openapi::schema::Type::String,
                )),
            )
            .item(utoipa::openapi::Ref::from_schema_name("McpToolExecutionError"))
            .item(
                utoipa::openapi::ObjectBuilder::new().schema_type(utoipa::openapi::schema::SchemaType::from_iter([
                    utoipa::openapi::schema::Type::Object,
                    utoipa::openapi::schema::Type::Number,
                    utoipa::openapi::schema::Type::Integer,
                    utoipa::openapi::schema::Type::Boolean,
                    utoipa::openapi::schema::Type::Array,
                ])),
            )
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for McpCallError {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("McpCallError")
    }
}

impl McpCallError {
    #[must_use]
    pub fn tool_execution(text: impl Into<String>) -> Self {
        Self::ToolExecution(McpToolExecutionError {
            type_: "mcp_tool_execution_error".to_owned(),
            content: vec![McpToolExecutionErrorContent {
                type_: "text".to_owned(),
                text: text.into(),
                annotations: None,
                meta: None,
            }],
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpToolExecutionError {
    #[serde(rename = "type")]
    pub type_: String,
    pub content: Vec<McpToolExecutionErrorContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpToolExecutionErrorContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
    pub annotations: Option<Value>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    pub id: String,
    pub server_label: String,
    pub name: String,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<McpCallStatus>,
    pub approval_request_id: Option<String>,
    pub output: Option<String>,
    pub error: Option<McpCallError>,
}

impl McpCall {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        server_label: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
        status: McpCallStatus,
        output: Option<String>,
        error: Option<McpCallError>,
    ) -> Self {
        Self {
            agent: None,
            id: id.into(),
            server_label: server_label.into(),
            name: name.into(),
            arguments: arguments.into(),
            status: Some(status),
            approval_request_id: None,
            output,
            error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpListTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

impl McpListTool {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        input_schema: Value,
        annotations: Option<Value>,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            input_schema,
            annotations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpListTools {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    pub id: String,
    pub server_label: String,
    pub tools: Vec<McpListTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl McpListTools {
    #[must_use]
    pub fn new(id: impl Into<String>, server_label: impl Into<String>, tools: Vec<McpListTool>) -> Self {
        Self {
            agent: None,
            id: id.into(),
            server_label: server_label.into(),
            tools,
            error: None,
        }
    }
}

impl TryFrom<&EventPayload> for McpListTools {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("mcpl_")
        } else {
            item_id.clone()
        };
        Ok(Self::new(id, "", vec![]))
    }
}

impl TryFrom<&EventPayload> for McpCall {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, name, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("mcp_")
        } else {
            item_id.clone()
        };
        Ok(Self::new(
            id,
            "",
            name.as_deref().unwrap_or_default(),
            "",
            McpCallStatus::InProgress,
            None,
            None,
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReasoningTextContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
}

impl ReasoningTextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            type_: "reasoning_text".into(),
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReasoningOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_nullable_vec")]
    pub content: Vec<ReasoningTextContent>,
    #[serde(default, deserialize_with = "deserialize_nullable_vec")]
    pub summary: Vec<Value>,
    pub encrypted_content: Option<Value>,
    pub status: Option<String>,
}

fn deserialize_nullable_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

impl ReasoningOutput {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            agent: None,
            id: id.into(),
            content: vec![],
            summary: vec![],
            encrypted_content: None,
            status: None,
        }
    }
}

impl TryFrom<&EventPayload> for ReasoningOutput {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        match payload {
            EventPayload::OutputItemAdded { item_id, .. } => {
                let id = if item_id.is_empty() {
                    uuid7_str("rs_")
                } else {
                    item_id.clone()
                };
                Ok(Self::new(id))
            }
            EventPayload::OutputItemDone { item, .. } => {
                let Some(OutputItem::Reasoning(item)) = deserialize_from_value_opt::<OutputItem>(item.clone()) else {
                    return Err(ExecutorError::ParseError(
                        "expected a complete reasoning output item".into(),
                    ));
                };
                if item.id.is_empty() {
                    return Err(ExecutorError::ParseError(
                        "complete reasoning output item is missing its id".into(),
                    ));
                }
                Ok(item)
            }
            _ => Err(ExecutorError::ParseError(
                "expected a reasoning output-item lifecycle payload".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "multi_agent_call")]
    MultiAgentCall(MultiAgentCall),
    #[serde(rename = "multi_agent_call_output")]
    MultiAgentCallOutput(MultiAgentCallOutput),
    #[serde(rename = "agent_message")]
    AgentMessage(AgentMessage),
    #[serde(rename = "message")]
    Message(OutputMessage),
    #[serde(rename = "function_call")]
    FunctionCall(FunctionToolCall),
    #[serde(rename = "tool_search_call")]
    ToolSearchCall(ToolSearchCall),
    #[serde(rename = "custom_tool_call")]
    CustomToolCall(CustomToolCall),
    #[serde(rename = "shell_call")]
    ShellCall(ShellCall),
    #[serde(rename = "web_search_call")]
    WebSearchCall(WebSearchCall),
    #[serde(rename = "mcp_call")]
    McpCall(McpCall),
    #[serde(rename = "mcp_list_tools")]
    McpListTools(McpListTools),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningOutput),
    #[serde(rename = "compaction")]
    Compaction(CompactionItem),
    #[serde(other)]
    Unknown,
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for OutputItem {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::Ref;
        use utoipa::openapi::schema::{AllOfBuilder, ObjectBuilder, OneOfBuilder, SchemaType, Type};

        fn tagged(type_value: &str, schema: &str) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
            AllOfBuilder::new()
                .item(
                    ObjectBuilder::new()
                        .property(
                            "type",
                            ObjectBuilder::new()
                                .schema_type(SchemaType::new(Type::String))
                                .enum_values(Some([type_value])),
                        )
                        .required("type"),
                )
                .item(Ref::from_schema_name(schema))
                .into()
        }

        OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(tagged("message", "OutputMessage"))
            .item(tagged("function_call", "FunctionToolCall"))
            .item(tagged("tool_search_call", "ToolSearchCall"))
            .item(tagged("custom_tool_call", "CustomToolCall"))
            .item(tagged("shell_call", "ShellCall"))
            .item(tagged("web_search_call", "WebSearchCall"))
            .item(tagged("mcp_call", "McpCall"))
            .item(tagged("mcp_list_tools", "McpListTools"))
            .item(tagged("reasoning", "ReasoningOutput"))
            .item(tagged("compaction", "CompactionItem"))
            .item(tagged("multi_agent_call", "MultiAgentCall"))
            .item(tagged("multi_agent_call_output", "MultiAgentCallOutput"))
            .item(tagged("agent_message", "AgentMessage"))
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for OutputItem {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("OutputItem")
    }
}

impl OutputItem {
    #[must_use]
    pub fn agent(&self) -> Option<&AgentAttribution> {
        match self {
            Self::MultiAgentCall(item) => item.agent.as_ref(),
            Self::MultiAgentCallOutput(item) => item.agent.as_ref(),
            Self::AgentMessage(item) => item.agent.as_ref(),
            Self::Message(item) => item.agent.as_ref(),
            Self::FunctionCall(item) => item.agent.as_ref(),
            Self::ToolSearchCall(item) => item.agent.as_ref(),
            Self::CustomToolCall(item) => item.agent.as_ref(),
            Self::ShellCall(item) => item.agent.as_ref(),
            Self::WebSearchCall(item) => item.agent.as_ref(),
            Self::McpCall(item) => item.agent.as_ref(),
            Self::McpListTools(item) => item.agent.as_ref(),
            Self::Reasoning(item) => item.agent.as_ref(),
            Self::Compaction(item) => item.agent.as_ref(),
            Self::Unknown => None,
        }
    }

    /// Set gateway-owned attribution after model ingestion.
    pub fn set_agent(&mut self, agent: AgentAttribution) {
        let slot = match self {
            Self::MultiAgentCall(item) => &mut item.agent,
            Self::MultiAgentCallOutput(item) => &mut item.agent,
            Self::AgentMessage(item) => &mut item.agent,
            Self::Message(item) => &mut item.agent,
            Self::FunctionCall(item) => &mut item.agent,
            Self::ToolSearchCall(item) => &mut item.agent,
            Self::CustomToolCall(item) => &mut item.agent,
            Self::ShellCall(item) => &mut item.agent,
            Self::WebSearchCall(item) => &mut item.agent,
            Self::McpCall(item) => &mut item.agent,
            Self::McpListTools(item) => &mut item.agent,
            Self::Reasoning(item) => &mut item.agent,
            Self::Compaction(item) => &mut item.agent,
            Self::Unknown => return,
        };
        *slot = Some(agent);
    }

    #[must_use]
    pub fn requires_client_action(&self, registry: &ToolRegistry) -> bool {
        match self {
            Self::FunctionCall(call) => registry
                .lookup(&call.name)
                .is_none_or(|entry| !entry.ownership.is_gateway()),
            Self::ToolSearchCall(_) | Self::CustomToolCall(_) | Self::ShellCall(_) => true,
            Self::Message(_)
            | Self::WebSearchCall(_)
            | Self::McpCall(_)
            | Self::McpListTools(_)
            | Self::Reasoning(_)
            | Self::Compaction(_)
            | Self::MultiAgentCall(_)
            | Self::MultiAgentCallOutput(_)
            | Self::AgentMessage(_)
            | Self::Unknown => false,
        }
    }

    /// Shapes a stored output item as continuation input.
    /// Public output for gateway-executed built-in tools is omitted because the model-facing
    /// function call and output are persisted separately as input items. MCP
    /// list metadata is retained here and removed by `ResponsesInput::model_input`.
    #[must_use]
    pub fn to_input_item(&self) -> Option<InputItem> {
        match self {
            Self::Message(message) => Some(InputItem::Message(message.clone().into())),
            Self::Reasoning(reasoning) => Some(InputItem::Reasoning(reasoning.clone())),
            Self::FunctionCall(call) => Some(InputItem::FunctionCall(InputFunctionToolCall::from(call.clone()))),
            Self::ToolSearchCall(call) => InputToolSearchCall::try_from(call).ok().map(InputItem::ToolSearchCall),
            Self::CustomToolCall(call) => Some(InputItem::FunctionCall(call.clone().into())),
            Self::ShellCall(call) => Some(InputItem::FunctionCall(call.clone().into())),
            Self::McpListTools(list_tools) => Some(InputItem::McpListTools(list_tools.clone())),
            Self::Compaction(item) => Some(InputItem::Compaction(item.clone())),
            Self::MultiAgentCall(item) => Some(InputItem::MultiAgentCall(item.clone())),
            Self::MultiAgentCallOutput(item) => Some(InputItem::MultiAgentCallOutput(item.clone())),
            Self::AgentMessage(item) => Some(InputItem::AgentMessage(item.clone())),
            Self::WebSearchCall(_) | Self::McpCall(_) | Self::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::InputItem;

    #[test]
    fn message_content_requires_a_known_type() {
        let part = serde_json::json!({"type": "unsupported", "text": ""});
        assert!(serde_json::from_value::<OutputMessageContent>(part).is_err());
    }

    #[test]
    fn attributed_message_preserves_text_metadata_on_continuation() {
        for content in [
            serde_json::json!({"type": "input_text", "text": "Review", "extension": "keep"}),
            serde_json::json!({"type": "output_text", "text": "OK"}),
            serde_json::json!({"type": "output_text", "text": "OK", "annotations": [], "logprobs": []}),
            serde_json::json!({"type": "output_text", "text": "OK",
                "annotations": [{"type": "url_citation", "url": "https://example.com", "title": "Source", "start_index": 0, "end_index": 2}],
                "logprobs": [{"token": "OK", "bytes": [79, 75], "logprob": -0.5,
                    "top_logprobs": [{"token": "NO", "bytes": [78, 79], "logprob": -1.5}]}]}),
        ] {
            let wire = serde_json::json!({"type": "message", "id": "msg_child",
                "role": if content["type"] == "input_text" { "user" } else { "assistant" },
                "agent": {"agent_name": "/root/review"}, "status": "completed", "content": [content]});
            let item: OutputItem = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&item).unwrap(), wire);
            assert_eq!(serde_json::to_value(item.to_input_item().unwrap()).unwrap(), wire);
        }
    }

    #[test]
    fn existing_item_kinds_preserve_optional_agent_attribution() {
        for mut wire in [
            serde_json::json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{}"}),
            serde_json::json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "custom", "input": "x"}),
            serde_json::json!({"type": "tool_search_call", "id": "tsc_1", "call_id": "call_1", "execution": "client", "arguments": {}, "status": "completed"}),
            serde_json::json!({"type": "shell_call", "id": "sh_1", "call_id": "call_1", "action": {"commands": ["pwd"]}}),
            serde_json::json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "docs", "queries": [], "sources": []}}),
            serde_json::json!({"type": "mcp_call", "id": "mcp_1", "server_label": "docs", "name": "search", "arguments": "{}"}),
            serde_json::json!({"type": "mcp_list_tools", "id": "mcpl_1", "server_label": "docs", "tools": []}),
            serde_json::json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "opaque"}),
            serde_json::json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "opaque"}),
        ] {
            let unattributed: OutputItem = serde_json::from_value(wire.clone()).unwrap();
            assert!(serde_json::to_value(unattributed).unwrap().get("agent").is_none());
            wire["agent"] = serde_json::json!({"agent_name": "/root/review"});
            let attributed: OutputItem = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&attributed).unwrap()["agent"], wire["agent"]);
            if let Some(input) = attributed.to_input_item() {
                assert_eq!(serde_json::to_value(input).unwrap()["agent"], wire["agent"]);
            }
        }
    }

    #[test]
    fn output_message_preserves_phase_independently_of_completed_status() {
        for (phase, expected) in [
            (None, None),
            (Some("commentary"), Some(MessagePhase::Commentary)),
            (Some("final_answer"), Some(MessagePhase::FinalAnswer)),
        ] {
            let mut wire = serde_json::json!({
                "type": "message",
                "id": "msg_phase",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "Review result.", "annotations": []}]
            });
            if let Some(phase) = phase {
                wire["phase"] = serde_json::json!(phase);
            }
            let item: OutputItem = serde_json::from_value(wire.clone()).unwrap();
            let OutputItem::Message(message) = &item else {
                panic!("expected a message item");
            };
            assert_eq!(message.phase, expected);
            assert_eq!(message.status, MessageStatus::Completed);
            let replay = item.to_input_item().unwrap();
            let InputItem::Message(replayed_message) = &replay else {
                panic!("expected a continuation message");
            };
            assert_eq!(replayed_message.phase, expected);
            assert_eq!(serde_json::to_value(replay).unwrap().get("phase"), wire.get("phase"));
            assert_eq!(serde_json::to_value(item).unwrap(), wire);
        }
    }

    #[test]
    fn forked_user_message_remains_input_text_on_continuation() {
        let output: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": "msg_forked",
            "role": "user",
            "status": "completed",
            "content": [
                {"type": "input_text", "text": "Review the parser."},
                {"type": "input_text", "text": "Check error handling."}
            ]
        }))
        .unwrap();

        assert!(serde_json::to_value(&output).unwrap().get("phase").is_none());
        let input = output.to_input_item().unwrap();
        assert_eq!(
            serde_json::to_value(input).unwrap(),
            serde_json::json!({
                "type": "message",
                "id": "msg_forked",
                "role": "user",
                "status": "completed",
                "content": [
                    {"type": "input_text", "text": "Review the parser."},
                    {"type": "input_text", "text": "Check error handling."}
                ]
            })
        );
    }

    #[test]
    fn assistant_message_remains_output_text_on_continuation() {
        let mut output = OutputMessage::new("msg_answer", MessageStatus::Completed);
        assert!(output.phase.is_none());
        output
            .content
            .push(OutputTextContent::new("The parser handles errors correctly.").into());

        let input = OutputItem::Message(output).to_input_item().unwrap();
        assert_eq!(
            serde_json::to_value(input).unwrap(),
            serde_json::json!({
                "type": "message",
                "id": "msg_answer",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "The parser handles errors correctly.", "annotations": []}]
            })
        );
    }

    #[test]
    fn emitted_tool_search_call_is_explicit_and_requires_client_action() {
        let wire = serde_json::json!({
            "type": "tool_search_call",
            "id": "provider_item_1",
            "call_id": "call_search_1",
            "execution": "client",
            "arguments": ["weather", "timezone"],
            "status": "completed"
        });
        let item: OutputItem = serde_json::from_value(wire.clone()).expect("valid emitted search call");

        assert_eq!(serde_json::to_value(&item).expect("call serializes"), wire);
        assert!(item.requires_client_action(&ToolRegistry::default()));
        let replay = item.to_input_item().expect("search call must remain public on replay");
        assert_eq!(serde_json::to_value(replay).expect("replay serializes"), wire);
    }

    #[test]
    fn emitted_tool_search_call_rejects_missing_or_invalid_required_fields() {
        for missing in ["execution", "arguments", "status"] {
            let mut wire = serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "execution": "client",
                "arguments": {"query": "weather"},
                "status": "completed"
            });
            wire.as_object_mut().expect("object").remove(missing);

            assert!(
                serde_json::from_value::<OutputItem>(wire).is_err(),
                "newly emitted calls require explicit {missing}"
            );
        }

        for (field, value) in [("id", serde_json::json!("   ")), ("call_id", serde_json::json!("   "))] {
            let mut wire = serde_json::json!({
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "execution": "client",
                "arguments": {"query": "weather"},
                "status": "completed"
            });
            wire[field] = value;

            assert!(
                serde_json::from_value::<OutputItem>(wire).is_err(),
                "newly emitted calls reject invalid {field}"
            );
        }
    }

    #[test]
    fn unfinished_tool_search_call_statuses_deserialize_but_are_not_replayable() {
        for status in ["in_progress", "incomplete"] {
            let item: OutputItem = serde_json::from_value(serde_json::json!({
                "type": "tool_search_call",
                "id": "provider_item_1",
                "call_id": "call_search_1",
                "execution": "client",
                "arguments": {},
                "status": status
            }))
            .unwrap();

            assert!(item.to_input_item().is_none());
        }
    }

    #[test]
    fn compaction_output_item_round_trips_with_type_tag() {
        let item: OutputItem = serde_json::from_value(serde_json::json!({
            "id": "cmp_1",
            "type": "compaction",
            "encrypted_content": "durable summary"
        }))
        .unwrap();

        assert!(!item.requires_client_action(&ToolRegistry::default()));
        let Some(InputItem::Compaction(compaction)) = item.to_input_item() else {
            panic!("compaction should rehydrate as a compaction input item");
        };
        assert_eq!(compaction.id.as_deref(), Some("cmp_1"));
        assert_eq!(compaction.encrypted_content, "durable summary");

        let serialized = serde_json::to_value(&item).unwrap();
        assert_eq!(serialized["type"], "compaction");
        assert_eq!(serialized["encrypted_content"], "durable summary");
        let parsed: OutputItem = serde_json::from_value(serialized).unwrap();
        assert!(matches!(parsed, OutputItem::Compaction(_)));
    }

    #[test]
    fn output_item_exposes_its_wire_id() {
        let item: OutputItem = serde_json::from_value(serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": []
        }))
        .expect("valid message output item");

        assert_eq!(item.id(), Some("msg_1"));
        assert_eq!(OutputItem::Unknown.id(), None);
    }

    #[test]
    fn custom_tool_call_preserves_freeform_input_and_requires_client_action() {
        let item: OutputItem = serde_json::from_value(serde_json::json!({
            "id": "ctc_1",
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": "call_1",
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** End Patch"
        }))
        .unwrap();

        assert!(item.requires_client_action(&ToolRegistry::default()));
        let OutputItem::CustomToolCall(call) = &item else {
            panic!("expected custom tool call");
        };
        assert_eq!(call.status, Some(MessageStatus::Completed));

        let Some(InputItem::FunctionCall(call)) = item.to_input_item() else {
            panic!("custom call should rehydrate as a function call");
        };
        assert_eq!(call.name, "apply_patch");
        assert_eq!(call.arguments, r#"{"input":"*** Begin Patch\n*** End Patch"}"#);
    }

    #[test]
    fn shell_call_round_trips_and_rehydrates_as_function_input() {
        let item: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "shell_call",
            "id": "sh_1",
            "call_id": "call_1",
            "action": {
                "commands": ["pwd"],
                "timeout_ms": 1000,
                "max_output_length": 4096
            },
            "status": "completed"
        }))
        .unwrap();

        assert!(item.requires_client_action(&ToolRegistry::default()));
        let Some(InputItem::FunctionCall(call)) = item.to_input_item() else {
            panic!("shell call should rehydrate as a function call input item");
        };
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "shell");
        assert_eq!(call.id.as_deref(), Some("fc_1"));
        let action: serde_json::Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(action["commands"], serde_json::json!(["pwd"]));

        let serialized = serde_json::to_value(item).unwrap();
        assert_eq!(serialized["type"], "shell_call");
    }

    #[test]
    fn web_search_call_rejects_empty_queries() {
        let error = WebSearchCall::try_new("ws_1", WebSearchCallStatus::Completed, Vec::new(), Vec::new()).unwrap_err();

        assert_eq!(error, WebSearchActionError::EmptyQueries);
    }

    #[test]
    fn web_search_call_preserves_valid_search_action_wire_shape() {
        let call = WebSearchCall::try_new(
            "ws_1",
            WebSearchCallStatus::Completed,
            vec!["rust async".to_owned()],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(call).unwrap(),
            serde_json::json!({
                "id": "ws_1",
                "status": "completed",
                "action": {
                    "type": "search",
                    "query": "rust async",
                    "queries": ["rust async"]
                }
            })
        );
    }

    #[test]
    fn gateway_public_tool_outputs_are_not_replayed_as_model_input() {
        let web_search = OutputItem::WebSearchCall(
            WebSearchCall::try_new(
                "ws_1",
                WebSearchCallStatus::Completed,
                vec!["rust async".to_owned()],
                Vec::new(),
            )
            .unwrap(),
        );
        let mcp = OutputItem::McpCall(McpCall::new(
            "mcp_1",
            "counter",
            "increment",
            "{}",
            McpCallStatus::Completed,
            Some("1".to_owned()),
            None,
        ));

        assert!(web_search.to_input_item().is_none());
        assert!(mcp.to_input_item().is_none());
    }

    #[test]
    fn custom_tool_call_status_remains_optional_on_the_wire() {
        let call: CustomToolCall = serde_json::from_value(serde_json::json!({
            "id": "ctc_1",
            "call_id": "call_1",
            "name": "apply_patch",
            "input": "patch"
        }))
        .unwrap();

        assert_eq!(call.status, None);
        let serialized = serde_json::to_value(call).unwrap();
        assert!(serialized.get("status").is_none());
    }

    #[test]
    fn reasoning_output_round_trips_through_serde() {
        let json = serde_json::json!({
            "id": "rs_abc",
            "type": "reasoning",
            "summary": [],
            "content": [{"text": "Let me think...", "type": "reasoning_text"}],
            "encrypted_content": null,
            "status": null
        });
        let item: OutputItem = serde_json::from_value(json).unwrap();
        assert!(matches!(item, OutputItem::Reasoning(_)));
        if let OutputItem::Reasoning(r) = &item {
            assert_eq!(r.id, "rs_abc");
            assert_eq!(r.content.len(), 1);
            assert_eq!(r.content[0].text, "Let me think...");
        }
        let serialized = serde_json::to_value(&item).unwrap();
        assert_eq!(serialized["type"], "reasoning");
        assert_eq!(serialized["id"], "rs_abc");
    }

    #[test]
    fn reasoning_output_builds_from_added_and_applies_indexed_done_events() {
        let added = EventPayload::OutputItemAdded {
            initial_item: None,
            item_id: "rs_1".to_owned(),
            item_type: crate::events::SSEItemType::Reasoning,
            output_index: Some(2),
            name: None,
            namespace: None,
            call_id: None,
        };
        let mut item = ReasoningOutput::try_from(&added).unwrap();

        for (content_index, text) in [(1, "second thought"), (0, "first thought")] {
            item.apply_done(
                &EventPayload::ReasoningTextDone {
                    text: text.to_owned(),
                    item_id: "rs_1".to_owned(),
                    output_index: Some(2),
                    content_index,
                },
                &mut String::new(),
            );
        }
        for (summary_index, text) in [(1, "second summary"), (0, "first summary")] {
            item.apply_done(
                &EventPayload::ReasoningSummaryTextDone {
                    text: text.to_owned(),
                    item_id: "rs_1".to_owned(),
                    output_index: Some(2),
                    summary_index,
                },
                &mut String::new(),
            );
        }

        assert_eq!(item.id, "rs_1");
        assert_eq!(
            item.content.iter().map(|part| part.text.as_str()).collect::<Vec<_>>(),
            ["first thought", "second thought"]
        );
        assert_eq!(item.summary[0]["text"], "first summary");
        assert_eq!(item.summary[1]["text"], "second summary");
    }

    #[test]
    fn reasoning_output_done_owns_authoritative_field_reconciliation() {
        let mut item = ReasoningOutput::new("rs_1");
        item.content.push(ReasoningTextContent::new("buffered thought"));
        item.summary
            .push(serde_json::json!({"type": "summary_text", "text": "buffered summary"}));
        let done = EventPayload::OutputItemDone {
            item_id: "rs_1".to_owned(),
            item_type: crate::events::SSEItemType::Reasoning,
            output_index: Some(0),
            item: serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "summary": null,
                "encrypted_content": "opaque-state",
                "status": "completed",
            }),
        };

        let parsed = ReasoningOutput::try_from(&done).unwrap();
        assert!(parsed.content.is_empty());
        assert!(parsed.summary.is_empty());

        item.apply_done(&done, &mut String::new());
        assert_eq!(item.content[0].text, "buffered thought");
        assert!(item.summary.is_empty());
        assert_eq!(item.encrypted_content, Some(serde_json::json!("opaque-state")));
        assert_eq!(item.status.as_deref(), Some("completed"));

        let before = serde_json::to_value(&item).unwrap();
        let malformed = EventPayload::OutputItemDone {
            item_id: "rs_1".to_owned(),
            item_type: crate::events::SSEItemType::Reasoning,
            output_index: Some(0),
            item: serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "content": "not-an-array",
            }),
        };
        item.apply_done(&malformed, &mut String::new());
        assert_eq!(serde_json::to_value(item).unwrap(), before);
    }

    #[test]
    fn reasoning_done_text_is_authoritative_even_when_empty() {
        let mut item = ReasoningOutput::new("rs_1");
        let mut stale_delta = "partial reasoning".to_owned();

        item.apply_done(
            &EventPayload::ReasoningTextDone {
                text: String::new(),
                item_id: "rs_1".to_owned(),
                output_index: Some(0),
                content_index: 0,
            },
            &mut stale_delta,
        );

        assert!(item.content.is_empty());
        assert!(stale_delta.is_empty());

        let mut stale_summary_delta = "partial summary".to_owned();
        item.apply_done(
            &EventPayload::ReasoningSummaryTextDone {
                text: String::new(),
                item_id: "rs_1".to_owned(),
                output_index: Some(0),
                summary_index: 0,
            },
            &mut stale_summary_delta,
        );

        assert!(item.summary.is_empty());
        assert!(stale_summary_delta.is_empty());
    }

    #[test]
    fn reasoning_input_round_trips_through_serde() {
        let reasoning = ReasoningOutput::new("rs_1");
        let item = InputItem::Reasoning(reasoning);
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["type"], "reasoning");
        let back: InputItem = serde_json::from_value(json).unwrap();
        assert!(matches!(back, InputItem::Reasoning(_)));
    }

    #[test]
    fn mcp_call_serializes_as_openai_output_item() {
        let item = OutputItem::McpCall(McpCall::new(
            "mcp_1",
            "counter",
            "increment",
            "{}",
            McpCallStatus::Completed,
            Some("1".to_owned()),
            None,
        ));

        let json = serde_json::to_value(item).unwrap();
        assert_eq!(json["type"], "mcp_call");
        assert_eq!(json["id"], "mcp_1");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["server_label"], "counter");
        assert_eq!(json["name"], "increment");
        assert_eq!(json["arguments"], "{}");
        assert_eq!(json["output"], "1");
        assert!(json["approval_request_id"].is_null());
        assert!(json["error"].is_null());
    }

    #[test]
    fn mcp_list_tools_serializes_as_openai_output_item() {
        let item = OutputItem::McpListTools(McpListTools::new(
            "mcpl_1",
            "counter",
            vec![McpListTool::new(
                "increment",
                Some("Increment the counter by one".to_owned()),
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
                Some(serde_json::json!({"read_only": false})),
            )],
        ));

        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "mcp_list_tools",
                "id": "mcpl_1",
                "server_label": "counter",
                "tools": [{
                    "name": "increment",
                    "description": "Increment the counter by one",
                    "input_schema": {
                        "type": "object",
                        "properties": {},
                    },
                    "annotations": {"read_only": false},
                }],
            })
        );

        let decoded: OutputItem = serde_json::from_value(json).unwrap();
        let OutputItem::McpListTools(decoded) = decoded else {
            panic!("expected MCP list-tools item");
        };
        assert_eq!(decoded.id, "mcpl_1");
        assert_eq!(decoded.server_label, "counter");
        assert_eq!(decoded.tools.len(), 1);
        assert_eq!(decoded.tools[0].name, "increment");
        assert!(decoded.error.is_none());
    }

    #[test]
    fn mcp_list_tools_builds_from_added_and_applies_done_item() {
        let added = EventPayload::OutputItemAdded {
            initial_item: None,
            item_id: "mcpl_1".to_owned(),
            item_type: crate::events::SSEItemType::McpListTools,
            output_index: Some(0),
            name: None,
            namespace: None,
            call_id: None,
        };
        let mut item = McpListTools::try_from(&added).unwrap();
        assert_eq!(item.id, "mcpl_1");
        assert!(item.server_label.is_empty());
        assert!(item.tools.is_empty());

        let done = EventPayload::OutputItemDone {
            item_id: "mcpl_1".to_owned(),
            item_type: crate::events::SSEItemType::McpListTools,
            output_index: Some(0),
            item: serde_json::json!({
                "type": "mcp_list_tools",
                "id": "mcpl_1",
                "server_label": "counter",
                "tools": [{
                    "name": "increment",
                    "description": "Increment the counter by one",
                    "input_schema": {"type": "object", "properties": {}},
                    "annotations": {"read_only": false},
                }],
            }),
        };
        item.apply_done(&done, &mut String::new());

        assert_eq!(item.id, "mcpl_1");
        assert_eq!(item.server_label, "counter");
        assert_eq!(item.tools.len(), 1);
        assert_eq!(item.tools[0].name, "increment");
    }

    #[test]
    fn vllm_reasoning_response_deserializes() {
        let vllm_output = serde_json::json!([
            {
                "id": "rs_bb637a529f72b88d",
                "summary": [],
                "type": "reasoning",
                "content": [{"text": "2+2 is 4.", "type": "reasoning_text"}],
                "encrypted_content": null,
                "status": null
            },
            {
                "id": "msg_bb68f033f2ed1725",
                "content": [{"annotations": [], "text": "2+2 equals 4.", "type": "output_text"}],
                "role": "assistant",
                "status": "completed",
                "type": "message"
            }
        ]);
        let items: Vec<OutputItem> = serde_json::from_value(vllm_output).unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], OutputItem::Reasoning(_)));
        assert!(matches!(items[1], OutputItem::Message(_)));
    }

    #[test]
    fn codex_response_items_round_trip_supported_shapes() {
        let function_call = serde_json::json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "run",
            "namespace": "mcp__shell",
            "arguments": "{\"cmd\":\"pwd\"}",
            "status": "completed"
        });
        let item: OutputItem = serde_json::from_value(function_call).unwrap();
        if let OutputItem::FunctionCall(call) = &item {
            assert_eq!(call.namespace.as_deref(), Some("mcp__shell"));
            assert_eq!(call.name, "run");
        } else {
            panic!("expected function call");
        }
        assert_eq!(serde_json::to_value(&item).unwrap()["namespace"], "mcp__shell");

        let future_item = serde_json::json!({
            "type": "future_item",
            "id": "future_1",
            "payload": {"a": 1}
        });
        let item: OutputItem = serde_json::from_value(future_item).unwrap();
        assert!(matches!(item, OutputItem::Unknown));

        let unknown = serde_json::json!({"type": "new_item", "payload": {"a": 1}});
        let item: InputItem = serde_json::from_value(unknown).unwrap();
        assert!(matches!(item, InputItem::Unknown));
    }

    #[test]
    fn known_items_with_new_nested_content_preserve_message_with_unknown_part() {
        let message = serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "future_content",
                    "payload": "future_value"
                }
            ]
        });

        let item: InputItem = serde_json::from_value(message).unwrap();
        let InputItem::Message(message) = &item else {
            panic!("expected message item");
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("expected message parts");
        };
        assert!(matches!(parts.as_slice(), [InputContent::Unknown(kind)] if kind == "future_content"));
    }
}
