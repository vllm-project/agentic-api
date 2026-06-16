use serde_json::Value;

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
    WebSearchCallSearching,
    WebSearchCallCompleted,

    // Catch-all for unrecognized events
    Other,
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
        usage: Option<Value>,
    },

    /// `response.output_item.added`
    OutputItemAdded {
        item_id: String,
        item_type: String,
        output_index: u32,
        name: Option<String>,
        call_id: Option<String>,
    },

    /// `response.output_item.done`
    OutputItemDone {
        item_id: String,
        item_type: String,
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
}
