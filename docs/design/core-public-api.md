# Design: `agentic-core` Public API

> Status: Active — implementation in progress
> References: [ADR-03](../adr/ADR-03_gateway_integration.md), [Issue #42](https://github.com/vllm-project/agentic-api/issues/42), [Praxis #354](https://github.com/praxis-proxy/praxis/issues/354)
> Owner: @ashwing (tool dispatch, loop control, streaming tee) + @maralbahari (base loop, store integration)

---

## Foundation: PR #46

[PR #46](https://github.com/vllm-project/agentic-api/pull/46) by @maralbahari implements the base executor loop for text-only stateful conversations:

| Function | File | What it does |
|----------|------|--------------|
| `execute()` | `executor/engine.rs` | Entry point — rehydrate → infer → persist |
| `rehydrate_conversation()` | `executor/engine.rs` | Load history from store, build enriched request |
| `call_inference()` | `executor/engine.rs` | Returns `impl Stream` of SSE lines (sync fn, not async — stream is lazy) |
| `persist_response()` | `executor/engine.rs` | Save response + items to store (takes handlers as explicit params) |
| `ResponseAccumulator` | `executor/accumulator.rs` | SSE state machine — collects stream into ResponsePayload |
| `ExecutionContext` | `executor/request.rs` | Runtime deps: handlers, HTTP client, LLM URL |
| `RequestContext` | `executor/request.rs` | Per-turn state: original + enriched request, IDs |

This design builds on top of PR #46 — it does not duplicate or replace that work.

---

## What This Design Adds

The base loop handles text messages. This design extends it with:

1. **Tool dispatch** — detect function_call items in output, execute via traits, loop back
2. **Loop control** — `LoopDecision` enum driving re-entry with iteration limits
3. **Streaming tee** — forward SSE to client in real-time while accumulating for tool detection
4. **Extended SSE events** — function_call, reasoning, file_search, web_search event types
5. **Tool executor traits** — MCP, web_search, vector_store as pluggable implementations

---

## Implementation Phases

Each phase = one PR with tests. Phases are ordered by dependency.

### Phase 1: SSE Event Extension (lands after PR #46 merges)

**PR scope:** Extend `types/event.rs` and `executor/accumulator.rs` (both introduced by PR #46).

- Add ~12 new `SSEEventType` variants for function_call, reasoning, output_item lifecycle
- Modify `ResponseAccumulator::process_sse_line()` (private method — changes are within the same file) to detect and collect `OutputItem::FunctionCall` from streaming deltas
- Unit tests verifying accumulator correctly builds FunctionCall items from SSE deltas

**Note:** `accumulator.rs` and `types/event.rs` are introduced by PR #46. This phase lands immediately after PR #46 merges — it's a small, focused follow-up with no other dependencies.

```rust
// New variants in SSEEventType
pub enum SSEEventType {
    // Existing (PR #46)
    ResponseCreated,
    ResponseOutputItemAdded,
    ResponseOutputTextDelta,
    ResponseDone,
    // New — Phase 1
    ResponseOutputItemDone,        // detect completed tool calls
    FunctionCallArgumentsDelta,    // streaming function args
    FunctionCallArgumentsDone,     // complete function call
    ResponseOutputTextDone,        // text completion signal
    ContentPartAdded,
    ContentPartDone,
    ReasoningSummaryTextDelta,
    ReasoningSummaryTextDone,
    // Catch-all
    Other,
}
```

**Size:** ~200 lines | **Blocked by:** PR #46 merge | **Target:** 3rd merged PR (fast follow-up)

---

### Phase 2: Loop Control + Tool Dispatch (depends on PR #46)

**PR scope:** `executor/dispatch.rs`, `executor/tool_context.rs`, extend `engine.rs`.

Core contribution — the agentic loop re-entry mechanism:

```rust
pub enum LoopDecision {
    Continue(Vec<InputItem>),   // tool results to append, re-enter inference
    Done,                       // no tool calls, response is final
    Incomplete(String),         // max iterations or unrecoverable failure
}

pub async fn dispatch_tools(
    output: &[OutputItem],
    tool_ctx: &ToolContext,
    iteration: usize,
) -> ExecutorResult<LoopDecision>

/// Initially non-streaming only (returns Left). Streaming support added in Phase 3.
pub async fn execute_loop(
    request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    tool_ctx: &ToolContext,
) -> ExecutorResult<ResponsePayload>
```

`execute_loop` wraps PR #46's functions in a tool-dispatch loop:
1. Rehydrate (delegates to PR #46's `rehydrate_conversation`)
2. Call inference (delegates to PR #46's `call_inference` — returns stream lazily)
3. Accumulate response (via `ResponseAccumulator::from_stream`)
4. Check output for `OutputItem::FunctionCall` → `dispatch_tools` → loop or done
5. Persist final response (delegates to PR #46's `persist_response` with explicit handlers)

**Phase 2 is non-streaming only.** The tool loop inspects the full accumulated response before deciding. Streaming + tool dispatch (forwarding events to client while detecting tool calls) requires Phase 3's tee pattern.

`ToolContext` holds optional executor references:

```rust
pub struct ToolContext {
    pub mcp_executor: Option<Arc<dyn McpToolExecutor>>,
    pub web_search: Option<Arc<dyn WebSearchProvider>>,
    pub vector_store: Option<Arc<dyn VectorStoreClient>>,
    pub max_iterations: usize,
}
```

**Size:** ~400 lines | **Blocked by:** PR #46 merge | **Target:** first feature PR (Phase 2 of committer track)

---

### Phase 3: Streaming Tee (depends on PR #46)

**PR scope:** `executor/stream_tee.rs`, refactor `run_stream` path.

PR #46's streaming path accumulates everything before emitting to client. This replaces it with a tee:

```rust
pub struct StreamTee {
    client_tx: mpsc::Sender<String>,     // forward to client
    accumulator: ResponseAccumulator,     // detect tool calls
}

impl StreamTee {
    pub fn split(
        raw_stream: impl Stream<Item = Result<String, ExecutorError>>,
        conversation_id: Option<&str>,
    ) -> (BoxStream, impl Future<Output = ResponsePayload>)
}
```

Returns two handles:
- `BoxStream` — yields SSE events to client in real-time
- `Future<ResponsePayload>` — resolves when stream completes, contains accumulated output for tool detection

This enables the real-time streaming requirement from ADR-01 §3 — events should reach the client as they arrive, interleaved with the tool loop, rather than buffered until completion.

**Size:** ~300 lines | **Blocked by:** PR #46 merge | **Target:** feature PR

---

### Phase 4: Tool Executor Traits + Mock Implementations (depends on Phase 2)

**PR scope:** `executor/tools/` module.

```rust
// Native async traits (Rust 1.75+, no #[async_trait] boxing needed)
pub trait McpToolExecutor: Send + Sync {
    fn execute(
        &self,
        tool_name: &str,
        arguments: &Value,
        server_config: &Value,
    ) -> impl Future<Output = Result<Value, ExecutorError>> + Send;
}

pub trait WebSearchProvider: Send + Sync {
    /// context_size: "low" | "medium" | "high" — controls result verbosity
    fn search(
        &self,
        query: &str,
        context_size: &str,
    ) -> impl Future<Output = Result<Value, ExecutorError>> + Send;
}

pub trait VectorStoreClient: Send + Sync {
    fn search(
        &self,
        store_id: &str,
        query: &str,
        max_results: u32,
    ) -> impl Future<Output = Result<Vec<Value>, ExecutorError>> + Send;
}
```

This PR includes mock implementations for integration testing (in-memory tool executors that return canned responses). Real implementations (MCP client, Brave search, Qdrant) come in later PRs.

**Size:** ~500 lines | **Blocked by:** Phase 2 | **Target:** feature PR

---

## Design Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | `ToolContext` separate from `ExecutionContext` | Keeps PR #46's struct focused on inference; tool deps are additive |
| D2 | `LoopDecision` carries tool results directly | Avoids mutating shared state between dispatch and re-entry |
| D3 | Streaming tee as separate module, not refactor of accumulator | Preserves PR #46's non-streaming path unchanged |
| D4 | Traits for tool executors, not concrete types | Enables OGX (PR #34), mock testing, and future providers |
| D5 | Phase 1 is a fast follow-up to PR #46 | Small scope, lands immediately after merge — unblocks Phase 2 quickly |

---

## Praxis Filter Mapping

How the complete pipeline maps to @leseb's proposed filter chain:

| # | Praxis Filter | Core Function | Phase | Owner |
|---|---------------|---------------|-------|-------|
| 0 | `request_validate` | `validate_request()` | Future | — |
| 1 | `response_store` (init) | `init_store()` | Future | — |
| 2 | `rehydrate` | `rehydrate_conversation()` | PR #46 | @maralbahari |
| 3 | `file_resolve` | `resolve_files()` | Future | — |
| 4 | `tool_parse` | `parse_tools()` | Future | — |
| 5 | `responses_proxy` | `call_inference()` | PR #46 | @maralbahari |
| 6 | `stream_events` | `transform_stream()` / tee | Phase 3 | @ashwing |
| 7 | `tool_dispatch` | `dispatch_tools()` | Phase 2 | @ashwing |
| 8 | `mcp_tool` | `McpToolExecutor::execute()` | Phase 4 | @ashwing |
| 9 | `web_search` | `WebSearchProvider::search()` | Phase 4 | @ashwing |
| 10 | `file_search` | `VectorStoreClient::search()` | Phase 4 | @ashwing |
| 11 | `compact` | `compact_context()` | Future | — |
| 12 | `reasoning` | `summarize_reasoning()` | Future | — |
| 13 | `response_store` (resp) | `persist_response()` | PR #46 | @maralbahari |

---

## Open Questions

1. **`execute_loop` vs refactoring `execute`:** Should the loop wrapper be a new function or replace PR #46's `execute()`? Pending maralbahari's response on PR #46 review.
2. **Streaming tee ownership model:** `Arc<Mutex<>>` vs channel-based accumulation. Will prototype both in Phase 3 PR.
3. **ResponseStore trait unification:** PR #33 has separate `ConversationStore` + `ResponseStore`. Keep separate or unify? Defer until Phase 4 when we need to abstract over them.
