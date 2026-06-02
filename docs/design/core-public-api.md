# Design: `agentic-core` Public API

> Status: Draft — soliciting inline review
> References: [ADR-03](../adr/ADR-03_gateway_integration.md), [Issue #42](https://github.com/vllm-project/agentic-api/issues/42), [Praxis #354](https://github.com/praxis-proxy/praxis/issues/354)
> Owner: @ashwing

---

## Overview

`agentic-core` exposes the agentic loop as composable step functions. Each function is:
- Independently testable (no HTTP server needed)
- Wrappable in a Praxis `HttpFilter` (1:1 mapping per ADR-03)
- Callable directly in standalone mode via `execute()`

This design incorporates @leseb's expanded proposal (14 functions, 8 traits) and organizes it into implementation phases.

**Relationship to existing code:** PR #33 landed a concrete `ConversationStore` + `ResponseStore` with SQLx. This design defines the *orchestration-level* traits that wrap or delegate to those concrete stores. The implementation PRs will wire against PR #33's types directly.

**Step numbering:** Steps are numbered sequentially within each phase (A1, A2, ... B1, B2, ... C1, C2, ...). The Praxis filter mapping table at the end shows how these map to @leseb's filter chain order.

---

## Shared State: `AgenticState`

Mutable, request-scoped struct that flows through the entire loop. Each step reads some fields and writes others.

```rust
pub struct AgenticState {
    // Identity (set once by validate_request, read-only thereafter)
    pub response_id: String,
    pub conversation_id: Option<String>,
    pub tenant_id: Option<String>,
    pub model: String,
    pub previous_response_id: Option<String>,
    pub request: Value,  // original request preserved for forwarding

    // Routing flags (set once by validate_request)
    pub store_enabled: bool,
    pub stream_enabled: bool,
    pub background: bool,

    // Conversation (written by rehydrate, read by call_inference)
    pub messages: Vec<Value>,

    // Tools (written by dispatch_tools, read by call_inference on loop)
    pub tools: Vec<Value>,
    pub tool_choice: Value,
    pub tool_calls: Vec<Value>,

    // Output (written by transform_stream, read by persist)
    pub output_items: Vec<Value>,
    pub response_object: Value,
    pub usage: Value,
    pub status: ResponseStatus,

    // Loop control
    pub iteration: u32,

    // Runtime
    pub config: AgenticConfig,
}

pub struct AgenticConfig {
    pub max_tool_iterations: u32,
    pub compaction_enabled: bool,
    pub compaction_threshold_tokens: u64,
    pub reasoning_summary_enabled: bool,
    pub default_model: Option<String>,
}

impl Default for AgenticConfig {
    fn default() -> Self {
        Self {
            max_tool_iterations: 10,
            compaction_enabled: false,
            compaction_threshold_tokens: 100_000,
            reasoning_summary_enabled: false,
            default_model: None,
        }
    }
}

pub enum ResponseStatus {
    Queued,
    InProgress,
    Completed,
    Incomplete(String),
    Failed(String),
    Cancelled,
}

pub enum LoopDecision {
    Continue,
    Done,
    Incomplete(String),
}

pub enum BackendFormat {
    Responses,
    ChatCompletions,
}
```

---

## Step Functions

### Phase A: Core Loop

These are the minimum set needed for a working agentic loop.

#### A1: `validate_request`

```rust
pub fn validate_request(
    body: &Value,
    config: &AgenticConfig,
) -> Result<AgenticState, AgenticError>
```

Parse the incoming request. Extract routing flags (`stream`, `store`, `background`), `model`, `previous_response_id`, `conversation_id`. Generate `response_id`. Validate constraints (`background=true && store=false` is invalid). Return fully initialized `AgenticState`.

Does NOT validate inference-specific params (temperature, top_p, etc.) — those are the backend's responsibility.

**Gateway filter:** `request_validate`

---

#### A2: `rehydrate_conversation`

```rust
pub async fn rehydrate_conversation(
    state: &mut AgenticState,
    store: &dyn ResponseStore,
) -> Result<(), AgenticError>
```

Load conversation history from `previous_response_id` or `conversation_id`. Reconstruct message list. Append current input.

- If `previous_response_id`: fetch stored response + messages. Returns `AgenticError::Validation` if the previous response is incomplete/in-progress/cancelled (cannot continue from an unfinished response).
- If `conversation_id` only: fetch conversation messages
- If neither: pass current input through (messages extracted from `state.request`)

**Gateway filter:** `rehydrate`

---

#### A3: `call_inference`

```rust
pub async fn call_inference(
    state: &AgenticState,  // intentionally &, not &mut — inference is read-only on state
    client: &dyn InferenceClient,
) -> Result<(InferenceStream, BackendFormat), AgenticError>
```

Build the inference request from `state.messages` + `state.tools` + `state.tool_choice`. Delegate to `InferenceClient`. Return raw stream + format indicator. Takes `&AgenticState` (not `&mut`) because it only reads — state mutation happens in `transform_stream` downstream.

The `InferenceClient` implementation handles request format conversion (Responses vs Chat Completions) internally.

**Gateway filter:** `responses_proxy`

---

#### A4: `transform_stream`

```rust
/// Non-streaming: drains the stream, accumulates into state, returns collected events.
pub async fn transform_stream(
    state: &mut AgenticState,
    raw_stream: InferenceStream,
    format: BackendFormat,
) -> Result<Vec<ResponsesEvent>, AgenticError>

/// Streaming variant: returns a stream that yields events AND accumulates into shared state.
/// Note: exact ownership model (Arc<Mutex<>>, channel-based, or split-state) is TBD — see Open Question 5.
pub fn transform_stream_live(
    state: /* shared state handle — see Open Question 5 */,
    raw_stream: InferenceStream,
    format: BackendFormat,
) -> ResponsesEventStream

pub type ResponsesEventStream = Pin<Box<dyn Stream<Item = Result<ResponsesEvent, AgenticError>> + Send>>;
```

The SSE state machine. Parses raw bytes from the backend into typed `ResponsesEvent` values. Simultaneously accumulates into `state` (tool_calls, output_items, usage, status).

**Two variants:**
- `transform_stream` (non-streaming / tool-loop path): consumes the stream fully, populates `state`, returns collected events. Used by `execute()` in the tool loop where we need all events before deciding whether to dispatch tools.
- `transform_stream_live` (streaming to client): returns a new stream that yields events as they arrive while accumulating into shared state via `Arc<Mutex<AgenticState>>`. Used by `agentic-server` when forwarding SSE to clients in real-time.

When `format` is `Responses`: minimal transformation (assign sequence numbers).
When `format` is `ChatCompletions`: full transformation — all 24 Responses API event types (see [OpenAI Responses API streaming docs](https://platform.openai.com/docs/api-reference/responses/streaming)).

```rust
pub enum ResponsesEvent {
    // Response lifecycle
    ResponseCreated(Value),
    ResponseInProgress(Value),
    ResponseCompleted(Value),
    ResponseIncomplete(Value),
    ResponseFailed(Value),
    // Output items
    OutputItemAdded(Value),
    OutputItemDone(Value),
    // Content parts
    ContentPartAdded(Value),
    ContentPartDone(Value),
    // Text streaming
    OutputTextDelta { delta: String, sequence_number: u64 },
    OutputTextDone { text: String, annotations: Vec<Value>, sequence_number: u64 },
    OutputTextAnnotationAdded(Value),
    // Function calls
    FunctionCallArgumentsDelta { delta: String, sequence_number: u64 },
    FunctionCallArgumentsDone { arguments: String, sequence_number: u64 },
    // Refusal
    RefusalDelta { delta: String, sequence_number: u64 },
    RefusalDone { refusal: String, sequence_number: u64 },
    // Reasoning
    ReasoningDelta { delta: String, sequence_number: u64 },
    ReasoningDone(Value),
    ReasoningSummaryTextDelta { delta: String, sequence_number: u64 },
    ReasoningSummaryTextDone(Value),
    ReasoningSummaryPartAdded(Value),
    ReasoningSummaryPartDone(Value),
    // Error
    Error(Value),
}
```

**Gateway filter:** `stream_events`

---

#### A5: `dispatch_tools`

```rust
pub async fn dispatch_tools(
    state: &mut AgenticState,
    deps: &AgenticDeps,  // see "Dependency Bundle" section below
) -> Result<LoopDecision, AgenticError>
```

Classify each tool call in `state.tool_calls` and dispatch to the appropriate executor from `deps`:
- **Function tool** (client-side): add to output, return `Done`
- **MCP tool**: execute via `deps.mcp_executor`
- **web_search**: execute via `deps.web_search`
- **file_search**: execute via `deps.vector_store`

Tool executors that aren't configured (None in deps) skip silently — the tool call is treated as client-side.

After server-side execution: append results to `state.messages`, increment `state.iteration`.

Returns `LoopDecision`:
- `Continue` — all server-side, results ready, loop back to inference
- `Done` — no tool calls, or client-side function calls present
- `Incomplete` — iteration limit or `finish_reason == "length"`

**Gateway filter:** `tool_dispatch` (with branch chain for loop control)

---

#### A6: `persist_response`

```rust
pub async fn persist_response(
    state: &AgenticState,  // &, not &mut — persistence is read-only on state
    store: &dyn ResponseStore,
) -> Result<(), AgenticError>
```

Save final response + messages to store. Skip when `state.store_enabled` is false. Takes `&AgenticState` (not `&mut`) because it only reads accumulated state — no mutations at this point.

**Gateway filter:** `response_store` (response phase)

---

#### A7 (standalone only): `execute`

```rust
pub async fn execute(
    body: Value,
    config: AgenticConfig,
    deps: &AgenticDeps,
) -> Result<Value, AgenticError>
```

Standalone entry point. Composes Phase A steps with default loop logic:

```text
state = validate_request(body, config)?
rehydrate_conversation(&mut state, &*deps.store).await?

loop {
    let (stream, format) = call_inference(&state, &*deps.inference).await?
    let _events = transform_stream(&mut state, stream, format).await?
    // state.tool_calls, state.output_items, state.usage now populated

    match dispatch_tools(&mut state, &deps).await? {
        LoopDecision::Continue => continue,
        LoopDecision::Done => break,
        LoopDecision::Incomplete(r) => { state.status = Incomplete(r); break; }
    }
}

persist_response(&state, &*deps.store).await?
Ok(state.response_object)
```

Note: `execute` uses the non-streaming `transform_stream` variant. For streaming to clients, `agentic-server` uses `transform_stream_live` and drives the event stream directly.

---

### Phase B: Tool Executors

Trait implementations for the tool dispatch layer. Phase A defines the trait signatures; Phase B provides concrete implementations.

Note: `HashMap` below refers to `std::collections::HashMap<String, String>` (used for HTTP headers in MCP server configs).

#### B1: `McpToolExecutor`

```rust
#[async_trait]
pub trait McpToolExecutor: Send + Sync {
    /// Execute an MCP tool call. Session management is internal to the implementation.
    async fn execute(
        &self,
        tool_name: &str,
        arguments: &Value,
        server_config: &Value,
    ) -> Result<Value, AgenticError>;
}
```

Implementations manage MCP sessions internally (create/reuse/close keyed on endpoint + headers). The session management traits below are implementation details, not part of the public API:

```rust
// Internal to MCP executor implementations — not in public API
pub trait McpSessionManager: Send + Sync {
    fn get_or_create_session(
        &self,
        endpoint_key: &str,
        server_url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Arc<dyn McpSession>, AgenticError>;
}

#[async_trait]
pub trait McpSession: Send + Sync {
    async fn call_tool(&self, name: &str, arguments: &Value) -> Result<Value, AgenticError>;
    async fn close(&self) -> Result<(), AgenticError>;
}
```

#### B2: `McpToolProvider`

```rust
#[async_trait]
pub trait McpToolProvider: Send + Sync {
    async fn list_tools(
        &self,
        server_url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Vec<Value>, AgenticError>;
}
```

#### B3: `WebSearchProvider`

```rust
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    async fn search(&self, query: &str, context_size: ContextSize) -> Result<Value, AgenticError>;
}

pub enum ContextSize { Low, Medium, High }
```

#### B4: `VectorStoreClient`

```rust
#[async_trait]
pub trait VectorStoreClient: Send + Sync {
    async fn search(
        &self,
        store_id: &str,
        query: &str,
        options: &FileSearchOptions,
    ) -> Result<Vec<Value>, AgenticError>;
}

pub struct FileSearchOptions {
    pub max_num_results: u32,
    pub filters: Option<Value>,
    pub ranking_options: Option<Value>,
}
```

---

### Phase C: Advanced Features

Optional steps that enhance the loop but aren't required for MVP.

**Note on ordering:** C1 (`init_store`) and C3 (`parse_tools`) logically run early in the pipeline (before inference). In Phase A without them, `dispatch_tools` works because it reads `state.tool_calls` populated by `transform_stream` from the LLM's output — tool *definitions* are forwarded as-is in `state.tools` from the original request. `parse_tools` adds MCP listing and normalization on top of that basic passthrough.

#### C1: `init_store`

```rust
pub async fn init_store(
    state: &AgenticState,
    store: &dyn ResponseStore,
) -> Result<(), AgenticError>
```

Create initial response record (status=queued for background, status=in_progress otherwise). Runs before rehydration so the response ID is persisted early.

#### C2: `resolve_files`

```rust
pub async fn resolve_files(
    state: &mut AgenticState,
    file_store: &dyn FileStore,
) -> Result<(), AgenticError>
```

Walk `state.messages`, resolve `file_id` references to inline content via `FileStore` trait.

#### C3: `parse_tools`

```rust
pub async fn parse_tools(
    state: &mut AgenticState,
    mcp_provider: &dyn McpToolProvider,
) -> Result<(), AgenticError>
```

Parse tool definitions. For MCP: call `tools/list`, build tool map. Normalize `tool_choice`. Writes to `state.tools` and `state.tool_choice`.

#### C4: `compact_context` (opt-in via config)

```rust
pub async fn compact_context(
    state: &mut AgenticState,
    inference: &dyn InferenceClient,
    store: &dyn ResponseStore,
) -> Result<bool, AgenticError>
```

Token counting + summarization. Only runs when `config.compaction_enabled` is true and threshold is exceeded. Returns `true` if compaction occurred.

#### C5: `summarize_reasoning` (opt-in via config)

```rust
pub async fn summarize_reasoning(
    state: &mut AgenticState,
    inference: &dyn InferenceClient,
) -> Result<Option<Vec<ResponsesEvent>>, AgenticError>
```

Post-streaming reasoning summary generation. Only runs when `config.reasoning_summary_enabled` is true. Runs after the tool loop completes.

#### C6: `FileStore` trait

```rust
#[async_trait]
pub trait FileStore: Send + Sync {
    async fn get_file(
        &self,
        file_id: &str,
    ) -> Result<Option<FileContent>, AgenticError>;
}

pub struct FileContent {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub filename: Option<String>,
}
```

---

## Dependency Bundle

```rust
pub struct AgenticDeps {
    // Required (Phase A)
    pub store: Arc<dyn ResponseStore>,
    pub inference: Arc<dyn InferenceClient>,

    // Phase B: tool executors (None = tool calls treated as client-side)
    pub mcp_executor: Option<Arc<dyn McpToolExecutor>>,
    pub web_search: Option<Arc<dyn WebSearchProvider>>,
    pub vector_store: Option<Arc<dyn VectorStoreClient>>,

    // Phase C: advanced features
    pub file_store: Option<Arc<dyn FileStore>>,
    pub mcp_provider: Option<Arc<dyn McpToolProvider>>,
}
```

Only `store` and `inference` are required. A minimal deployment (proxy + persistence, no server-side tools) only needs those two. All `Option` fields are populated as their respective phases are implemented — the struct definition is stable across all phases.

**Note on `McpSessionManager`:** Session lifecycle is internal to the `McpToolExecutor` implementation. The executor manages its own session pool — callers don't need to provide sessions explicitly. This keeps the public API surface simple while allowing different session strategies per implementation.

---

## Traits

### `ResponseStore`

The orchestration-level store trait. PR #33's concrete `ConversationStore` + `ResponseStore` types are the first implementation. This trait abstracts over them for the step functions.

```rust
#[async_trait]
pub trait ResponseStore: Send + Sync {
    async fn get_response(&self, response_id: &str) -> Result<Option<Value>, AgenticError>;
    async fn insert_response(&self, response: &Value) -> Result<(), AgenticError>;
    async fn update_response(&self, response_id: &str, update: &Value) -> Result<(), AgenticError>;

    async fn get_messages(&self, response_id: &str) -> Result<Option<Vec<Value>>, AgenticError>;
    async fn store_messages(&self, response_id: &str, messages: &[Value]) -> Result<(), AgenticError>;

    async fn list_input_items(
        &self, response_id: &str, limit: u32, cursor: Option<&str>,
    ) -> Result<Value, AgenticError>;
}
```

**Note:** PR #33 has `ConversationStore` (conversation-level ops) and `ResponseStore` (response-level ops) as separate concrete types. The orchestration trait above unifies them — the implementation delegates to both under the hood. Whether to keep this unified or split into two traits is an open question for review.

Implementations: SQLx wrapper around PR #33 (default), OGX (PR #34), InMemory (testing).

### `InferenceClient`

```rust
#[async_trait]
pub trait InferenceClient: Send + Sync {
    async fn call(
        &self,
        request: &Value,
        config: &AgenticConfig,
    ) -> Result<(InferenceStream, BackendFormat), AgenticError>;
}

pub type InferenceStream = Pin<Box<dyn Stream<Item = Result<Bytes, AgenticError>> + Send>>;
```

### `AgenticError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum AgenticError {
    #[error("validation: {0}")]
    Validation(String),

    #[error("store: {0}")]
    Store(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("inference: {0}")]
    Inference(String),

    #[error("inference timeout after {timeout_s}s")]
    InferenceTimeout { timeout_s: f64 },

    #[error("tool dispatch: {tool_name}: {message}")]
    ToolDispatch { tool_name: String, message: String },

    #[error("max iterations ({max}) reached")]
    MaxIterations { max: u32 },

    #[error("response not found: {0}")]
    NotFound(String),

    #[error("stream transform: {0}")]
    StreamTransform(String),
}
```

---

## Gateway Integration (Praxis)

Each step function maps to exactly one Praxis filter. The "Filter #" column shows the order in the Praxis filter chain (from @leseb's proposal):

| Step | Core Function | Praxis Filter # | Praxis Filter Name | Phase |
|------|---------------|----------------|--------------------|-------|
| A1 | `validate_request()` | 0 | `request_validate` | A |
| A2 | `rehydrate_conversation()` | 2 | `rehydrate` | A |
| A3 | `call_inference()` | 5 | `responses_proxy` | A |
| A4 | `transform_stream()` | 6 | `stream_events` | A |
| A5 | `dispatch_tools()` | 7 | `tool_dispatch` | A |
| A6 | `persist_response()` | 13 | `response_store` | A |
| B1 | `McpToolExecutor::execute()` | 8 | `mcp_tool` | B |
| B3 | `WebSearchProvider::search()` | 9 | `web_search` | B |
| B4 | `VectorStoreClient::search()` | 10 | `file_search` | B |
| C1 | `init_store()` | 1 | `response_store` (init) | C |
| C2 | `resolve_files()` | 3 | `file_resolve` | C |
| C3 | `parse_tools()` | 4 | `tool_parse` | C |
| C4 | `compact_context()` | 11 | `compact` | C |
| C5 | `summarize_reasoning()` | 12 | `reasoning` | C |

**Note:** Phase B entries (B1, B3, B4) are trait method calls invoked internally by A5 (`dispatch_tools`). In Praxis, @leseb's proposal exposes them as separate filters (8, 9, 10) for per-tool-type observability and independent configuration. B2 (`McpToolProvider`) has no corresponding filter — it's a setup-time operation used by C3 (`parse_tools`).

Tool dispatch uses Praxis branch chains for loop control:
```yaml
- filter: tool_dispatch
  branch_chains:
    - name: tool-loop
      on_result: { filter: tool_dispatch, key: action, result: loop }
      rejoin: responses_proxy
      max_iterations: 10
```

---

## Open Questions

1. **Compact/reasoning as opt-in or mandatory?** Currently proposed as opt-in via `AgenticConfig` flags. If mandatory, they add latency on every request.
2. **Per-tool-type executors: public functions or trait methods?** This proposal keeps them as trait methods called by `dispatch_tools`. Alternative: expose as standalone functions per leseb's proposal.
3. **Praxis #354 status:** Is the filter decomposition accepted? Affects how tightly we couple step numbering.
4. **ResponseStore: unified or split?** PR #33 has separate `ConversationStore` + `ResponseStore`. Should the orchestration trait unify them or keep them separate?
5. **`transform_stream` borrow strategy:** Need to validate in Rust whether `&mut state` + returned stream works, or if we need interior mutability (`RefCell`/`Mutex`) or a consume-and-return pattern.
6. **`AgenticState` field visibility:** All fields are `pub` for simplicity. Should we add accessor methods to enforce read/write contracts per step?
