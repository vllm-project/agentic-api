# Architecture

This document explains how `agentic-api` is put together: the crate boundaries, the
request lifecycle, and where to make a change for common contribution tasks. It
assumes you've read the crate overview in [AGENTS.md](AGENTS.md) and complements it —
AGENTS.md covers tooling and conventions, this document covers the mental model.

Background on *why* the system is shaped this way lives in the ADRs
([ADR-01](docs/adr/ADR-01_core.md), [ADR-02](docs/adr/ADR-02_response_store.md),
[ADR-03](docs/adr/ADR-03_gateway_integration.md)) and the design docs under
`docs/design/`. Those documents record proposals and drift over time; this document
describes the code as it exists today and will be kept current as the code moves.
Where a design doc's "as-built" notes and the code agree, this document just states
the outcome.

## Workspace layout

```
agentic-api/
  crates/
    agentic-server-core/   # "agentic_core" — pure Rust orchestration library
    agentic-server/        # axum HTTP/WS gateway + the `agentic` CLI launcher
    agentic-praxis/        # placeholder: future Praxis gateway adapter
```

- **`agentic-server-core`** (library crate name `agentic_core`) is where all domain
  logic lives: request/response types, SSE parsing, the agentic loop, tool framework,
  and the storage layer. It has no HTTP framework dependency.
- **`agentic-server`** is a thin transport layer: an axum binary that parses HTTP/WS,
  calls into `agentic_core`, and streams the result back. It also happens to host a
  second, unrelated binary — a CLI launcher (`agentic`) that spawns the gateway and a
  coding harness (Codex/Claude Code) as subprocesses for local use.
- **`agentic-praxis`** is currently a placeholder. Per ADR-03, the intent is for it to
  wrap each `agentic-server-core` public function as an `HttpFilter` so Praxis can
  compose the agentic loop declaratively instead of going through `agentic-server`'s
  axum router. Nothing is implemented there yet.

The dependency direction is one-way: `agentic-server` depends on `agentic-server-core`,
never the reverse. `agentic-server-core` has no knowledge of axum, HTTP, or WebSockets.

## Request flow at a glance

```
Client ──HTTP/WS──▶ agentic-server (handler/*)
                        │
                        ▼
              agentic_core::executor::ExecuteRequest::run()
                        │
          ┌─────────────┼──────────────────────────┐
          ▼             ▼                           ▼
     rehydrate()   upstream call(s) + tool loop   persist()
   (storage read)  (vLLM, gateway tool execution)  (storage write)
                        │
                        ▼
                 SSE / JSON back to client
```

Persistence uses `sqlx` against a driver-agnostic `Any` pool backed by SQLite or
Postgres (`storage::pool`). The upstream inference call targets vLLM's own stateless
Responses API — this project owns the state, vLLM owns tokenization and generation
(see ADR-01 §1.1).

## `agentic-server` — the transport layer

### Two binaries sharing one library

The crate produces a library plus two independent binaries. `src/lib.rs` exports
`agentic_cli`, `agentic_harness`, `agentic_output`, `agentic_process`, `app`, `auth`,
and `handler`. On top of that:

| Binary | Entry point | Uses |
|---|---|---|
| `agentic-server` (the gateway) | `src/main.rs` | `app`, `auth`, `handler`, plus binary-private `server.rs` and `config_file.rs` |
| `agentic` (the CLI launcher) | `src/bin/agentic.rs` | `agentic_cli`, `agentic_harness`, `agentic_output`, `agentic_process` only |

These are two unrelated concerns bundled in one crate. If you're working on request
handling, ignore `agentic_cli*`/`agentic_harness.rs`/`agentic_output.rs`/
`agentic_process.rs` entirely — they're the launcher that spawns the gateway binary and
a coding harness (Codex or Claude Code) as subprocesses for local, single-command use
(`agentic serve <model>`), and never touch the request path.

### `app.rs`, `server.rs`, `main.rs`

- **`app.rs`** (library) builds the router: `AppState` (the per-request-shared state:
  `exec_ctx: Arc<ExecutionContext>`, proxy state, readiness/websocket trackers, config)
  and `build_router_with_auth(state, server_config, authenticator)`, which wires every
  route and optionally layers OIDC auth (`auth::require_oidc`) onto the protected ones.
- **`server.rs`** (binary-private) owns process lifecycle: `build_state` (constructs
  `ExecutionContext::from_config`, i.e. where the DB pool actually gets created),
  `serve_gateway`/`serve_gateway_until_signal` (bind, serve, graceful shutdown with a
  bounded drain), and `run`/`run_with_llm` (standalone mode, optionally spawning a vLLM
  subprocess).
- **`main.rs`** (binary-private) is the `clap` CLI front end: parses config from
  flags/env/`config.toml`, then calls `server::run` or `server::run_with_llm`.

### HTTP handlers (`handler/http/`)

| Route | Handler | File |
|---|---|---|
| `POST /v1/responses` | `responses` | `handler/http/responses.rs` |
| `POST /v1/responses/compact` | `compact_response` | `handler/http/responses.rs` |
| `POST /v1/conversations` | `conversations` | `handler/http/conversations.rs` |
| `POST /v1/messages` | `messages` | `handler/http/messages.rs` |
| `POST /v1/messages/count_tokens` | `count_tokens` | `handler/http/messages.rs` |
| `GET /v1/models` | `models` | `handler/http/models.rs` |
| `GET /health` | `health` | `handler/http/models.rs` |
| `GET /ready` | `ready` | `handler/http/models.rs` |

Handlers make a request-scoped decision between two paths:
- **Stateful/executor path** — used when the request needs state (`store: true`,
  `previous_response_id`, `conversation_id`, compaction, or a gateway-owned tool).
  Builds an `ExecuteRequest` (Responses) or calls `run_messages_loop`/
  `run_messages_stream` (Messages) against `state.exec_ctx`.
- **Transparent proxy path** — everything else is forwarded to vLLM unchanged via
  `agentic_core::proxy`, with no state, no persistence.

### WebSocket transport (`handler/websocket/`)

`GET /v1/responses` upgrades to a WebSocket. Structurally this is not a one-shot
handler like the HTTP routes — `responses_ws_loop` is a long-lived session loop that
reads `response.create` messages off the socket, queues any that arrive while a
response is streaming, and drives the *same* `ExecuteRequest::run()` executor call the
HTTP handler uses. WebSocket sessions always force `stream: true, store: true`. Because
axum's built-in graceful shutdown doesn't wait for upgraded connections, `AppState`
carries a separate `WebSocketTracker` so shutdown can drain in-flight sessions.
Errors are modeled by a dedicated `WsError` enum (`handler/websocket/error.rs`) rather
than reusing the HTTP JSON-error path, since some failure modes (a dead socket) must
not attempt to write a response.

### `handler/common.rs`

Transport-agnostic helpers shared by both HTTP and WS handlers: body reading with a
shared size cap, `RequestPayload` parsing, bearer-token extraction, SSE response
wrapping, and rendering an `ExecutorError` as a JSON error body.

### `auth.rs`

OIDC bearer-token authentication: discovery, JWKS fetch/cache/refresh, and the
`require_oidc` axum middleware layered onto protected routes in `app.rs`. The
WebSocket handler also reads the resulting `AuthenticatedPrincipal` extension directly,
to detect token expiry mid-session.

### The hard boundary: no direct storage access

**Nothing in `agentic-server`'s request-handling code (`handler/*`, `app.rs`,
`server.rs`) imports `agentic_core::storage` directly.** All persistence goes through
`AppState.exec_ctx: Arc<ExecutionContext>` — e.g. `ExecuteRequest::run()`,
`create_conversation()`, `persist_turn()`, `rehydrate_conversation()`,
`ExecutionContext::storage_ready()`. `ExecutionContext::from_config` is the only place
that constructs the storage handlers, and it does so precisely so callers don't need
to depend on the storage layer:

```rust
let conv_handler = ConversationHandler::new(ConversationStore::new(pool.clone()));
let resp_handler = ResponseHandler::new(ResponseStore::new(pool.clone()));
```

Two narrow, deliberate exceptions: the `agentic validate` CLI subcommand
(`src/bin/agentic.rs`) calls `storage::create_pool_with_schema` directly as a
connectivity pre-flight check, outside the request path; and integration tests /
benches under `crates/agentic-server/{tests,benches}` import `ConversationStore`/
`ResponseStore` directly for fixture setup and assertions. Production request-handling
code should never do either.

## `agentic-server-core` — the orchestration core

Per [AGENTS.md](AGENTS.md), the internal dependency direction is: `types/` owns
wire/domain data → `events/` parses upstream events → `tool/` owns tool discovery,
routing, and execution → `executor/` orchestrates across inference, tools, and
storage → `storage/` owns persistence. Handlers call executor APIs; the executor
coordinates `events`, `tool`, and `storage`; those share contracts through `types`.

In `src/` code, reuse `utils::common` for JSON serialization/deserialization and
fallback behavior. Do not call `serde_json` directly when an existing strict,
optional, or defaulting helper expresses the required policy; add a focused helper
there when the policy is reused. Direct `serde_json` use is fine in tests, fixtures,
and cassette tooling. Keep Serde wire-format attributes on the owning type.

### `types/` — wire shapes, not behavior

This module's job is JSON ⇄ Rust type conversion and shape validation for the
Responses and Messages APIs. It is not where tool execution, state transitions, or DB
access happen — those live in `tool/`, `executor/`, and `storage/` respectively.

- **`types/request_response.rs`** — `RequestPayload` is the deserialized incoming
  request. Its `to_upstream_request(&self, stream: bool) -> Result<UpstreamRequest<'_>, ToolError>`
  is the seam between the OpenAI-shaped request and vLLM's contract. It: flattens Codex
  namespace tool members to model-visible names, validates every declared tool
  (`ResponsesTool::validate()`), normalizes every tool kind to `UpstreamTool::Function`
  (`ResponsesTool::to_function_tools()` — **every** tool type the model sees is
  `type: "function"`, because that's the only type vLLM speaks), and resolves/validates
  `tool_choice`. It's called from `executor/upstream.rs`'s `fetch_blocking_payload` and
  `fetch_stream_payload` — the two functions that actually build the outbound request
  to vLLM.
- **`types/io/`** — `input.rs` (inbound message/tool-call/tool-result shapes,
  `ResponsesInput`), `output.rs` (outbound output items: messages, function calls, web
  search/MCP calls, reasoning — plus the `ApplyDone` trait described below), `tools.rs`
  (the normalized `FunctionTool` and `ToolChoice`, distinct from tool *declarations*),
  `usage.rs` (token accounting structs).
- **`types/tools/params.rs`** — the tool **declaration** shapes a client sends:
  `ResponsesTool` (tagged enum: `Function`, `ToolSearch`, `Mcp`, `WebSearch`, `FileSearch`,
  `CodeInterpreter`, `Namespace`, `Custom`, `Unknown`) and each variant's param struct.
  This is a good concrete example of the module boundary: `ResponsesTool` is *defined*
  here as a pure shape, but its behavior — `validate()` and `to_function_tools()` — is
  implemented as an `impl ResponsesTool` block physically living in
  `tool/normalize.rs`, which delegates to per-type handlers. Types own the shape; tool
  owns what it means.
- **`types/messages/`** — a separate, parallel type layer for the Anthropic Messages
  API (`MessagesRequest`, `ContentBlock`, etc.). `tool_seam.rs` is the pure, I/O-free
  adapter that converts Anthropic tool blocks into the same internal `ResponsesTool`/
  `FunctionToolCall` vocabulary the Responses-side `ToolRegistry` already understands,
  so both APIs share one tool-routing mechanism without the Messages loop depending on
  `RequestPayload`/`ResponsePayload`.
- **`types/event.rs`** — small status enums (`ResponseStatus`, `MessageStatus`).

### `events/` — parsing upstream SSE, and how to add a new event type

This module normalizes raw upstream SSE lines into typed frames, decoupled from the
executor so the accumulator doesn't do inline JSON parsing.

- **`types.rs`** — `SSEEventType` (the wire event's `type`, covering both OpenAI's and
  vLLM's naming, e.g. `response.done` vs `response.completed`), `EventPayload` (the
  typed, extracted payload — falls back to `Raw(Value)` for events not deeply parsed
  yet), `WireEvent` (the raw pass-through shape, used for re-serialization),
  `EventFrame { event_type, payload, wire }` (the normalized output), `SSEItemType`
  (output-item kind: reasoning, function call, MCP call, etc.).
- **`normalize.rs`** — `normalize_sse_line(&str) -> Option<EventFrame>` parses a
  `data: ...` line and classifies it; `extract_payload` dispatches to small per-event
  `extract_*` helpers.

**To add support for a new SSE event**, the touch points are, in order:
1. `events/types.rs` — add the `SSEEventType` variant, its wire-string mapping both
   directions, and (if it carries structured data) an `EventPayload` variant.
2. `events/normalize.rs` — extend `extract_payload` and add an `extract_*` helper if
   the payload needs real parsing (otherwise it can fall through to `Raw`).
3. `executor/accumulator.rs`'s `process_event` — add a match arm to fold the new event
   into accumulator state, the same way every existing streamed field does.
4. Only if the event is gateway-synthesized (a built-in tool's lifecycle event):
   `executor/gateway.rs`'s `synthetic_event` call sites, and if it's a function-call
   shaped event needing translation, `executor/function_sse.rs`.

### `executor/` — the loop, and the server's only door into storage

This is the layer `agentic-server` talks to. It owns the request lifecycle: rehydrate,
call inference, run the tool loop, persist. `agentic-server` never reaches past it.

- **`request.rs`** — `RequestContext` (per-turn state: original + enriched request,
  response/conversation IDs) and `ExecutionContext` (long-lived deps: storage
  handlers, HTTP client, gateway tool executors, LLM base URL). `ExecutionContext` is
  what `AppState` holds; it exposes `conv_handler`/`resp_handler` (the `modes/`
  handlers below), never the raw stores.
- **`rehydrate.rs`** — `rehydrate_conversation()` loads prior history from either the
  conversation store or the response store depending on which ID the request carries,
  and builds the enriched `RequestContext`.
- **`upstream.rs`** — `fetch_blocking_payload`/`fetch_stream_payload`: builds the
  `UpstreamRequest` (via `to_upstream_request`, see above) and drives one round of
  upstream inference, running the accumulator and `FunctionSseTranslator` over the
  response and feeding synthesized frames through `GatewayStreamAccumulator`.
- **`inference.rs`** — `call_inference()`: the raw HTTP/SSE transport to vLLM. No
  parsing beyond splitting `data: ...` lines and stopping at `[DONE]`.
- **`engine.rs`** — the top-level orchestrator: `ExecuteRequest`/`execute()`,
  `create_conversation()`, and — this is worth being precise about —
  **`run_gateway_tool_loop` is where the multi-round tool loop actually lives**, not in
  `gateway.rs`. It calls `upstream.rs` for each round, hands the resulting output to
  `gateway.rs`'s helpers, and uses `gateway::classify_round`'s `LoopDecision` to decide
  whether to loop again, finish, hand back to the client, or give up (capped at
  `MAX_GATEWAY_TOOL_ROUNDS = 10`). Also home to `run_compaction_trigger`,
  `run_blocking`, and `run_stream` (spawns the loop, forwards events as SSE, persists
  before yielding the terminal event).
- **`persist.rs`** — `persist_response`/`persist_turn`, which route to
  `ConversationHandler` or `ResponseHandler` in `modes/` depending on whether the turn
  is conversation-scoped or response-scoped.
- **`compaction.rs`** — `compact_response()` (the explicit `/v1/responses/compact`
  path) and `maybe_compact_context()` (automatic, threshold-triggered, called from the
  round loop before each inference call).
- **`modes/conversation.rs`, `modes/response.rs`** — `ConversationHandler` and
  `ResponseHandler`. Thin, 1:1 wrappers around `storage::ConversationStore` /
  `storage::ResponseStore` that translate `RequestContext` into store calls and
  `StorageError` into `ExecutorError`. **This is the sanctioned boundary between the
  executor and the storage stores** — nothing above this layer touches
  `storage::conversation`/`storage::response` directly. Today they only cover what the
  pipeline needs (`get`, `get_or_create`, `create`, `rehydrate[_snapshot]`,
  `execute_turn`, `validate_exists`); **any new CRUD operation beyond persist/rehydrate
  belongs here**, added as a new method that delegates to the corresponding store.
- **`error.rs`** — `ExecutorError`, with the mapping methods (`http_status()`,
  `error_type()`, `into_response_body()`, ...) handlers use to render errors.

#### `accumulator.rs` — `ResponseAccumulator`: a stability contract, not just a file

`ResponseAccumulator` is the SSE state machine that turns a stream of `EventFrame`s
into a `ResponsePayload`. Its public surface is intentionally small
(`new`, `from_json`, `from_stream`, `from_sse_lines`, `mark_incomplete`, `finalize`) and
**should not grow**. Don't add a new public method and call it from another method on
the struct — that's a surface API change. Extend behavior through the existing
pattern instead:

- Each output item arrives via `response.output_item.added` and is **parked** as an
  `InFlightEntry` in `self.in_flight: IndexMap<String, InFlightEntry>`, keyed by item
  ID, in insertion order.
- Items are **constructed** via real `TryFrom<&EventPayload>` impls
  (`ReasoningOutput::try_from`, `FunctionToolCall::try_from`, `CustomToolCall::try_from`,
  `OutputMessage::try_from`, `CompactionItem::try_from`, `McpCall::try_from`,
  `McpListTools::try_from`, all in `types/io/output.rs`) — not ad hoc field-by-field
  building in the accumulator.
- Streamed deltas mutate the parked entry's buffer in place.
- Items are **completed** via the `ApplyDone` trait
  (`fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String)`), applied on
  the matching `*_done` event or on `response.output_item.done`.
- Parked entries are promoted into the final `output: Vec<OutputItem>` only once, in
  `finalize_all`, which drains `in_flight`, calls each item's `finalize()`, sorts by
  `output_index`, and appends to the output — invoked at end-of-stream or on a terminal
  `response.completed|failed|incomplete` event.

If you're adding a new output-item kind: give it a `TryFrom<&EventPayload>` impl and an
`ApplyDone` impl in `types/io/output.rs`, and add it to the `InFlight` enum and the
`start_output_item`/`finalize` match arms in `accumulator.rs`. Don't restructure the
public methods to accommodate it.

#### `gateway_accumulator.rs` — `GatewayStreamAccumulator`

This is a smaller, different job than the name's similarity to `ResponseAccumulator`
suggests: it holds no `OutputItem`/in-flight item state at all. Its purpose is to make
several upstream rounds of gateway tool execution look like **one continuous SSE
stream** to the client — it assigns monotonically increasing `sequence_number`s across
rounds, rebases `output_index` so gateway-tool output lands after prior output, and
deduplicates `response.created`/`response.in_progress` so they fire once per response
rather than once per round. `gateway.rs` and `upstream.rs` both feed frames through it
via `process_event`/`synthetic_event`/`emit_sse_frame`.

#### `function_sse.rs` — `FunctionSseTranslator`

Upstreams without native support for a declared tool type emit `function_call` SSE
events instead. This translator borrows the request-scoped tool registry for
classification and reshapes those raw calls accordingly:
- **Custom tools** — rewritten into the public `custom_tool_call` event shape
  (`output_item.added` / `custom_tool_call_input.delta` / `.done` / `output_item.done`),
  reconstructing the `input` JSON incrementally from the streamed `arguments`.
- **Gateway-owned tools** (`Mcp`, `WebSearch`, `FileSearch`, `CodeInterpreter`) — raw
  frames are suppressed entirely. Their real client-visible events are synthesized
  later, once the call has actually executed, by `gateway.rs`.
- **Client-owned tools** (`Function`, `CodexNamespace`) — pass through unchanged.
- **Tool search** — native `tool_search_call` events pass through as typed items;
  synthetic `function_call` events named `tool_search` are projected into that same
  public lifecycle after validation.

It also buffers function-call events that arrive before the call's name is known
(bounded at 256 KiB) and replays them once the name resolves.

#### `gateway.rs` — the tool-loop's building blocks

As noted above, the round-by-round loop itself is `engine.rs::run_gateway_tool_loop`.
`gateway.rs` supplies what that loop calls each round:
- `classify_round(...) -> LoopDecision` — `Continue` / `Done` / `RequiresClientAction` /
  `Incomplete(reason)`. Client-owned calls take precedence: a round with both gateway
  and client calls still executes and records the gateway calls' outputs, but returns
  `RequiresClientAction` in that same round rather than a separate "partial" state.
- `execute_output_calls` — runs every gateway-owned call for the round **concurrently**,
  bounded by a sliding window (`MAX_CONCURRENT_GATEWAY_CALLS = 5`, via
  `futures::stream::buffered`), each individually timeout-bounded
  (`GATEWAY_TOOL_TIMEOUT = 60s`) by `execute_gateway_call`. Result order matches call
  order regardless of completion order.
- `gateway_event_plans` / `emit_gateway_start_events` / `emit_gateway_completed_events`
  — build and emit the synthetic "start" events for all planned calls up front, then
  the "completed"/"failed" events once execution finishes, through
  `GatewayStreamAccumulator`.
- `execute_and_emit_output_calls` composes the three steps above: plan → emit start →
  execute (concurrently) → emit completed.

#### `messages_loop.rs` / `messages_request.rs` / `messages_stream.rs`

A **parallel, independent implementation** of the same shape of loop for the Anthropic
Messages API. `messages_stream.rs`'s own header comment describes it as "structurally
the Anthropic-native analogue of `GatewayStreamAccumulator`, kept deliberately parallel
for a future consolidation" — it never touches `RequestPayload`/`ResponsePayload`/
`ResponseAccumulator`/`GatewayStreamAccumulator`/`FunctionSseTranslator`, operating
directly on Anthropic-shaped JSON. The two loops share only the protocol-neutral
pieces: `ToolRegistry::dispatch` and `types::messages::tool_seam`. The round/timeout
constants (`MAX_GATEWAY_TOOL_ROUNDS`, `GATEWAY_TOOL_TIMEOUT`) are duplicated and
manually kept in sync with the Responses-side ones rather than shared — a known seam,
not an oversight, per the future-consolidation note.

### `storage/` — persistence

- **`pool.rs`** — `DbPool = sqlx::Pool<sqlx::Any>`, driver-agnostic across SQLite and
  Postgres. `create_pool`/`create_pool_with_schema` and friends build and tune it
  (WAL mode + busy-timeout retry on SQLite, statement/lock timeouts on Postgres).
- **`backend.rs`** — `DatabaseBackend` (Postgres/Sqlite/Other) detection from a
  connection URL, plus URL redaction for safe logging.
- **`schema.rs`** — migrations and readiness (`PoolWithSchema::ensure_schema_ready`),
  including a path for a supervisor-managed schema that skips running migrations
  itself and just verifies compatibility.
- **`models/`** — raw `sqlx::FromRow` row structs per table (`Conversation`, `Item`,
  `Response`) plus their raw, transaction-aware SQL functions (`create_in_tx`, `get`,
  `lock_in_tx`, ...). This is the literal DB row shape: JSON columns are still strings
  here.
- **`types/`** — the conversion layer from those raw rows into business types, via
  `From`/`TryFrom` impls: `ConversationData`/`ConversationSnapshot`, `ResponseData`/
  `ResponseMetadata` (parses the JSON metadata column into a typed struct),
  `InOutItem` (parses an `Item.data` JSON blob back into a typed `InputItem` or
  `OutputItem`), and `StorageError`.
- **`conversation.rs`, `response.rs`** — `ConversationStore` and `ResponseStore`: the
  CRUD-with-transactions layer (`create`, `get`, `get_or_create`, `rehydrate[_snapshot]`,
  `persist`/`persist_if_version` — each transactional, via `pool.begin()` /
  `tx.commit()`). **These are not to be called outside `executor/` and `storage/`
  themselves.** The only sanctioned callers are `executor/modes/conversation.rs` and
  `executor/modes/response.rs`, described above. (Integration tests and benches import
  them directly for fixtures — that's expected and fine; production code paths should
  not.)

### `tool/` — the tool framework

Wire shapes for tool declarations live in `types::tools` (see above); this module owns
the behavioral layer — routing, handler traits, normalization, and execution.

- **`normalize.rs`** — the `impl ResponsesTool` block with `validate()` and
  `to_function_tools()`. Both are a match over the tool-kind enum that delegates to
  each type's `ToolHandler`: e.g. `Function` → `FunctionHandler`, `Mcp` → `McpHandler`,
  `Namespace` → `CodexNamespaceHandler`, `Custom` → `CustomHandler`. `WebSearch` is
  normalized inline from a static builder (single fixed tool, no per-instance state).
  `FileSearch`/`CodeInterpreter` are declared but currently normalize to nothing — no
  handler is registered yet.
- **`handler.rs`** — the two traits every tool type reasons about:
  ```rust
  pub trait ToolHandler: Send + Sync {
      fn tool_type(&self) -> ToolType;
      fn validate(&self, param: &Value) -> Result<(), ToolError>;
      fn normalize(&self, param: &Value) -> Vec<FunctionTool>;
  }

  pub trait GatewayExecutor: ToolHandler + 'static {
      fn execute(&self, call_id: &str, tool_name: &str, arguments: &str, config: &Value)
          -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>>;
  }
  ```
  `GatewayExecutor` requires `ToolHandler` — every gateway-owned tool is also a
  `ToolHandler`, but not every `ToolHandler` is gateway-executable.
  - **Client-owned** tools implement only `ToolHandler`: see `function.rs`
    (`FunctionHandler`), `custom.rs` (`CustomHandler`), `codex.rs`
    (`CodexNamespaceHandler`). Their calls come back as `status: "requires_action"` for
    the client to resolve — the gateway never executes them.
  - **Gateway-owned / built-in** tools implement both traits: see `web_search.rs`
    (`WebSearchHandler`, backed by You.com) and `mcp/handler.rs` (`McpHandler`, backed
    by `mcp/client.rs`'s MCP protocol client and `mcp/pool.rs`'s connection pool).
- **`registry.rs`** — `ToolRegistry`, a request-scoped map from model-visible tool name
  to `ToolEntry { tool_type, config, server_label, handler }`. Its constructor,
  ```rust
  pub async fn build_with_handlers(
      tools: &mut [ResponsesTool],
      executors: &mut GatewayExecutors,
  ) -> Result<Self, ToolError>
  ```
  is the stable entry point every caller (Responses and Messages) uses to build a
  registry for a request — **its signature should not change**. It resolves namespace
  members, inserts one entry per declared/discovered tool, and for `Mcp`/`WebSearch`
  pulls the actual executor from `GatewayExecutors` (discovering live MCP tools via
  `tools/list` in the process). `ToolRegistry::dispatch(call)` is the per-call routing
  method the tool loop uses to resolve and run one call.
- **`executors.rs`** — `GatewayExecutors`, a shared registry built once at startup and
  reused across requests, specifically for gateway tools that need **lazy, per-request
  connection setup**: MCP servers (connects and caches `McpClient`s keyed by server
  URL, falling back to connecting a fresh request-declared server) and the shared
  `WebSearchHandler`. As of today it only has slots for `ToolType::Mcp` and
  `ToolType::WebSearch` — `insert()` logs and no-ops for any other type. Client-owned
  tools (`function`, `custom`, `namespace`) never touch this file; their registry
  entries are inserted with `handler: None` and no `GatewayExecutors` involvement.

**To add a new tool type:**
1. Implement `ToolHandler` (validate + normalize) for it. If it's client-executed,
   stop there — see `function.rs`/`custom.rs` for the pattern.
2. If it's gateway-executed, also implement `GatewayExecutor::execute` — see
   `web_search.rs`/`mcp/handler.rs`.
3. Wire it into `tool/normalize.rs`'s `validate`/`to_function_tools` match arms.
4. Wire it into `tool/registry.rs`'s `build_with_handlers` (an `insert_*_entry` call).
5. If it needs lazy per-request connection setup, add a slot to `GatewayExecutors` in
   `tool/executors.rs` and reference it from the registry's match arm for that type.

## `agentic-praxis`

Currently a placeholder (`src/lib.rs` is a comment describing intent). Per ADR-03, this
crate will eventually provide `HttpFilter` implementations, one per
`agentic-server-core` public function, composed into a Praxis filter chain with branch
support for tool-call looping — an alternative orchestrator to `agentic-server`'s axum
router, reusing the same core logic in-process.

## Quick reference: "I want to..."

| Task | Where |
|---|---|
| Add a new HTTP or WebSocket route | `agentic-server/src/handler/{http,websocket}/`, wire it in `app.rs`'s `build_router_with_auth` |
| Support a new upstream SSE event | `events/types.rs` → `events/normalize.rs` → `executor/accumulator.rs` (+ `gateway.rs`/`function_sse.rs` if it's gateway-synthesized) |
| Add a new tool type | `tool/handler.rs` impl(s) → `tool/normalize.rs` → `tool/registry.rs` → `tool/executors.rs` if it needs lazy connection setup |
| Add a CRUD operation beyond persist/rehydrate | `executor/modes/conversation.rs` or `modes/response.rs`, backed by `storage/conversation.rs` / `storage/response.rs` |
| Change how output items are assembled from a stream | `executor/accumulator.rs` — respect the `TryFrom`/`ApplyDone` pattern, don't add new public methods |
| Add a new Responses/Messages wire field | `types/io/` or `types/messages/` — shape only, no behavior |

## Further reading

- [AGENTS.md](AGENTS.md) — module boundaries, lint/format rules, commit and PR conventions
- [TERMINOLOGY.md](TERMINOLOGY.md) — normative vocabulary for API/state/tool/streaming concepts
- [ROADMAP.md](ROADMAP.md) — project direction and near-term focus
- [docs/adr/](docs/adr/) — architecture decision records
- [docs/design/](docs/design/) — as-built design docs (tool framework, core public API, MCP integration, Codex integration)
