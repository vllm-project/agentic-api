use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::io::ResponseUsage;

/// The type of an output item received during streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SSEItemType {
    Reasoning,
    FunctionCall,
    CustomToolCall,
    WebSearchCall,
    McpToolCall,
    Message,
}

impl SSEItemType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::FunctionCall => "function_call",
            Self::CustomToolCall => "custom_tool_call",
            Self::WebSearchCall => "web_search_call",
            Self::McpToolCall => "mcp_tool_call",
            Self::Message => "message",
        }
    }
}

impl From<&str> for SSEItemType {
    fn from(s: &str) -> Self {
        match s {
            "reasoning" => Self::Reasoning,
            "function_call" => Self::FunctionCall,
            "custom_tool_call" => Self::CustomToolCall,
            "web_search_call" => Self::WebSearchCall,
            "mcp_tool_call" => Self::McpToolCall,
            _ => Self::Message,
        }
    }
}

impl From<String> for SSEItemType {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl PartialEq<str> for SSEItemType {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SSEItemType {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Classification of SSE event types from the Responses API.
///
/// Covers both the `OpenAI` and vLLM wire formats (e.g. `response.done` vs
/// `response.completed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SSEEventType {
    // Response lifecycle
    ResponseCreated,
    ResponseInProgress,
    ResponseCompleted,
    ResponseFailed,
    ResponseIncomplete,

    // Output item lifecycle
    OutputItemAdded,
    OutputItemDone,

    // Text content
    OutputTextDelta,
    OutputTextDone,
    ContentPartAdded,
    ContentPartDone,

    // Function calls
    FunctionCallArgumentsDelta,
    FunctionCallArgumentsDone,
    CustomToolCallInputDelta,
    CustomToolCallInputDone,

    // Reasoning
    ReasoningTextDelta,
    ReasoningTextDone,
    ReasoningPartAdded,
    ReasoningPartDone,
    ReasoningSummaryTextDelta,
    ReasoningSummaryTextDone,

    // Built-in tool calls
    FileSearchCallSearching,
    FileSearchCallCompleted,
    WebSearchCallInProgress,
    WebSearchCallSearching,
    WebSearchCallCompleted,
    McpToolCallInProgress,
    McpToolCallCompleted,

    // Catch-all for unrecognized events
    Other,
}

impl SSEEventType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResponseCreated => "response.created",
            Self::ResponseInProgress => "response.in_progress",
            Self::ResponseCompleted => "response.completed",
            Self::ResponseFailed => "response.failed",
            Self::ResponseIncomplete => "response.incomplete",
            Self::OutputItemAdded => "response.output_item.added",
            Self::OutputItemDone => "response.output_item.done",
            Self::OutputTextDelta => "response.output_text.delta",
            Self::OutputTextDone => "response.output_text.done",
            Self::ContentPartAdded => "response.content_part.added",
            Self::ContentPartDone => "response.content_part.done",
            Self::FunctionCallArgumentsDelta => "response.function_call_arguments.delta",
            Self::FunctionCallArgumentsDone => "response.function_call_arguments.done",
            Self::CustomToolCallInputDelta => "response.custom_tool_call_input.delta",
            Self::CustomToolCallInputDone => "response.custom_tool_call_input.done",
            Self::ReasoningTextDelta => "response.reasoning_text.delta",
            Self::ReasoningTextDone => "response.reasoning_text.done",
            Self::ReasoningPartAdded => "response.reasoning_part.added",
            Self::ReasoningPartDone => "response.reasoning_part.done",
            Self::ReasoningSummaryTextDelta => "response.reasoning_summary_text.delta",
            Self::ReasoningSummaryTextDone => "response.reasoning_summary_text.done",
            Self::FileSearchCallSearching => "response.file_search_call.searching",
            Self::FileSearchCallCompleted => "response.file_search_call.completed",
            Self::WebSearchCallInProgress => "response.web_search_call.in_progress",
            Self::WebSearchCallSearching => "response.web_search_call.searching",
            Self::WebSearchCallCompleted => "response.web_search_call.completed",
            Self::McpToolCallInProgress => "response.mcp_tool_call.in_progress",
            Self::McpToolCallCompleted => "response.mcp_tool_call.completed",
            Self::Other => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<u64>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl WireEvent {
    #[must_use]
    pub fn new(event_type: impl Into<String>) -> Self {
        Self {
            event_type: event_type.into(),
            sequence_number: None,
            output_index: None,
            rest: Map::new(),
        }
    }

    #[must_use]
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()))
    }

    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()))
    }
}

/// Typed payload extracted from an SSE event's JSON data.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EventPayload {
    /// `response.created` / `response.completed` / `response.failed` /
    /// `response.incomplete` / `response.in_progress`
    Response {
        id: String,
        status: String,
        usage: Option<ResponseUsage>,
    },

    /// `response.output_item.added`
    OutputItemAdded {
        item_id: String,
        item_type: SSEItemType,
        output_index: u32,
        name: Option<String>,
        namespace: Option<String>,
        call_id: Option<String>,
    },

    /// `response.output_item.done`
    OutputItemDone {
        item_id: String,
        item_type: SSEItemType,
        output_index: u32,
        item: Value,
    },

    /// `response.output_text.delta`
    TextDelta {
        delta: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
    },

    /// `response.output_text.done`
    TextDone {
        text: String,
        item_id: String,
        output_index: u32,
    },

    /// `response.function_call_arguments.delta`
    FunctionCallArgsDelta {
        delta: String,
        call_id: Option<String>,
        item_id: String,
        output_index: u32,
    },

    /// `response.function_call_arguments.done`
    FunctionCallArgsDone {
        arguments: String,
        call_id: Option<String>,
        item_id: String,
        name: String,
        output_index: u32,
    },

    /// `response.custom_tool_call_input.delta`
    CustomToolCallInputDelta {
        delta: String,
        item_id: String,
        output_index: u32,
    },

    /// `response.custom_tool_call_input.done`
    CustomToolCallInputDone {
        input: String,
        item_id: String,
        output_index: u32,
    },

    /// `response.reasoning_summary_text.delta`
    ReasoningDelta { delta: String, item_id: String },

    /// `response.reasoning_summary_text.done`
    ReasoningDone { text: String, item_id: String },

    /// Events we classify but don't deeply parse yet.
    Raw(Value),

    /// No meaningful payload (e.g. unparseable content).
    None,
}

/// A normalized SSE event frame — the output of [`normalize_sse_line`].
///
/// [`normalize_sse_line`]: crate::events::normalize::normalize_sse_line
#[derive(Debug, Clone)]
pub struct EventFrame {
    pub event_type: SSEEventType,
    pub payload: EventPayload,
    pub sequence_number: Option<u64>,
    pub wire: WireEvent,
}

impl EventFrame {
    #[must_use]
    pub fn synthetic(event_type: SSEEventType, mut wire: WireEvent) -> Self {
        event_type.as_str().clone_into(&mut wire.event_type);
        let payload = EventPayload::Raw(wire.to_value());
        Self {
            event_type,
            payload,
            sequence_number: wire.sequence_number,
            wire,
        }
    }
}
