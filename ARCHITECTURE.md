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
    agentic-llm-d/         # split-execution state backend for the llm-d coordinator
    agentic-praxis/        # placeholder: future Praxis gateway adapter
```

- **`agentic-server-core`** (library crate name `agentic_core`) is where all domain
  logic lives: request/response types, SSE parsing, the agentic loop, tool framework,
  and the storage layer. It has no HTTP framework dependency.
- **`agentic-server`** is a thin transport layer: an axum binary that parses HTTP/WS,
  calls into `agentic_core`, and streams the result back. It also happens to host a
  second, unrelated binary — a CLI launcher (`agentic`) that spawns the gateway and a
  coding harness (Codex/Claude Code) as subprocesses for local use.
- **`agentic-llm-d`** is a separate axum backend for the llm-d coordinator, which runs
  inference itself. Its [router](crates/agentic-llm-d/src/lib.rs) exposes
  `/v1alpha/responses/hydrate` and `/v1alpha/responses/persist`, plus health/readiness
  probes. It uses `agentic_core` for state services and does not proxy or call a model.
- **`agentic-praxis`** is currently a placeholder. Per ADR-03, the intent is for it to
  wrap each `agentic-server-core` public function as an `HttpFilter` so Praxis can
  compose the agentic loop declaratively instead of going through `agentic-server`'s
  axum router. Nothing is implemented there yet.

The dependency direction is one-way: `agentic-server` and `agentic-llm-d` depend on
`agentic-server-core`, never the reverse. `agentic-server-core` has no axum dependency;
client-facing HTTP/WS transport belongs to the adapter crates, while upstream HTTP/SSE
I/O lives in core inference transport.

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

One Responses turn may contain several inference rounds, but it is exposed and
persisted as one response:

```
rehydrate history
      │
      ▼
create AgentPipeline + prepare tool-search state
      │
      ▼
build EngineOrchestration (ToolRegistry + response budget)
      │
      ▼
┌─▶ optional compaction ─▶ one upstream inference round
│                              │
│                              ▼
│                    AgentPipeline ingests JSON or SSE
│                    and relays public stream events
│                              │
│                              ▼
│                    resolve calls by ownership
│                       │               │
│                       │               └─ client-owned calls stay unresolved
│                       ▼
│             GatewayScheduler plans and executes calls
│             with bounded fan-out and ordered results/events
│                       │
│                       ▼
│                  classify_round
│              │            │             │
│              │            │             └─ client/incomplete/done: finalize
│              │            └─ append calls/results to continuation input
└──────────────┘  (`Continue`, at most 10 rounds)
      │
      ▼
finalize one public response + persist one turn
```

A mixed round may contain both ownership classes. Gateway-owned calls still execute
and are recorded, while the response returns the unresolved client-owned calls for the
client to resolve. Streaming uses the same round loop and projects it through one
continuous SSE lifecycle.

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
- **Pass-through path** — everything else is forwarded to vLLM unchanged via
  `agentic_core::proxy`, with no state, no persistence.

The HTTP Responses handler first parses `RequestPayload<RawValue>` so routing fields
remain typed while provider-specific `text` formats stay opaque on the pass-through
path. Executor routes convert that raw field to `ResponseTextConfig` before
building `ExecuteRequest`, preserving strict validation for in-process execution.

### WebSocket transport (`handler/websocket/`)

`GET /v1/responses` upgrades to a WebSocket. Structurally this is not a one-shot
handler like the HTTP routes — `responses_ws_loop` is a long-lived session loop that
reads `response.create` messages off the socket and drives the *same*
`ExecuteRequest::run()` executor call the HTTP handler uses. Requests with distinct
`stream_id` values run concurrently, while requests in the same lane remain FIFO;
requests without a `stream_id` share a default FIFO lane. The session admits at most
64 active or queued requests and 12 MiB of aggregate request data. WebSocket sessions
always force `stream: true, store: true`. Because axum's built-in graceful shutdown
doesn't wait for upgraded connections, `AppState` carries a separate
`WebSocketTracker` so shutdown can drain in-flight sessions.

Executor streams propagate downstream backpressure through a bounded event channel.
Upstream SSE lines are capped at 256 KiB, while normalized events are capped at 1 MiB.
Each request also shares a 1 MiB response budget across MCP discovery, upstream rounds,
and normalized gateway tool output, so a slow consumer cannot turn a fixed event-count
buffer into unbounded retained memory. MCP discovery participates in the same 16-permit
materialization window as gateway calls and is capped at 64 server declarations and
128 discovered tools per request.
The WebSocket transport queues only serialized, size-checked events, capped at
1 MiB each including routing metadata, in its 64-entry outbound queue. Local
completion validates both lifecycle events before persistence. Authentication is
rechecked at request dispatch so queued work cannot start after identity expiry.
Errors are modeled by a dedicated `WsError` enum (`handler/websocket/error.rs`) rather
than reusing the HTTP JSON-error path, since some failure modes (a dead socket) must
not attempt to write a response.

### `handler/common.rs`

Transport helpers shared by the HTTP and WS handlers: body reading with a shared size
cap, generic JSON parsing, bearer-token extraction, SSE response wrapping, and
rendering an `ExecutorError` as a JSON error body.

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

### Prefer established Rust patterns

Before adding a conversion helper, wrapper API, or state abstraction, search the
owning types and the standard Rust traits for an existing pattern. Use the
[standard conversion traits](https://doc.rust-lang.org/std/convert/) and consult the
[Rust Cookbook](https://rust-lang-nursery.github.io/rust-cookbook/) for established
recipes for common operations. Prefer code whose ownership, failure mode, and
lifecycle are visible in its types.

- Implement `From<T> for U` for an infallible, obvious, and value-preserving consuming
  conversion. This provides `Into<U>` automatically; implement `From` rather than
  implementing `Into` directly.
- Implement `TryFrom<T> for U` when validation or conversion can fail. This provides
  `TryInto<U>` automatically and keeps the error type with the destination contract.
- Use `AsRef`, borrowed accessors, or typed views for cheap borrowed conversions that
  should not consume or clone the value.
- Use iterator adapters such as `map`, `filter_map`, `flat_map`, and `collect` for
  collection conversion. Keep the per-item conversion on the item type instead of
  duplicating a variant match in each caller.
- Use newtypes to establish validated identities, indexes, names, and framed values at
  construction. `OutputIndex`, `NonEmptyToolName`, and `SseLine` are examples in this
  codebase.
- Use enums and exhaustive matches for closed state machines, `Option`/`Result`
  combinators for explicit absence and failure, and RAII guards for cleanup or
  cancellation tied to ownership.

Search `types/io/` before introducing a helper named `convert_*`, `into_*`, or
`append_*`. Existing examples include `TryFrom<&EventPayload>` for typed output items,
`From` implementations between wire/domain types, and
`OutputItem::to_input_item` for the deliberately optional continuation conversion.
An inherent conversion method is appropriate when the operation has domain policy
that the standard trait cannot express clearly, as with output variants that are
intentionally omitted from continuation input.

### `types/` — wire shapes, not behavior

This module's job is JSON ⇄ Rust type conversion and shape validation for the
Responses and Messages APIs. It is not where tool execution, state transitions, or DB
access happen — those live in `tool/`, `executor/`, and `storage/` respectively.

- **`types/request_response.rs`** — `RequestPayload` is the deserialized incoming
  request. Its `to_upstream_request(&self, stream: bool) -> Result<UpstreamRequest<'_>, ToolError>`
  is the seam between the OpenAI-shaped request and vLLM's contract. It: flattens Codex
  namespace tool members to model-visible names, validates every declared tool
  (`ResponsesTool::validate()`), and normalizes each supported model-visible tool to
  `UpstreamTool::Function` (`ResponsesTool::to_function_tools()`). File search, code
  interpreter, and unknown typed declarations currently normalize to no upstream
  tool; every declaration that does reach vLLM is `type: "function"`, because that's
  the only tool type it speaks. The conversion also resolves/validates `tool_choice`
  and applies `ResponsesInput::model_input()`. It's called from
  `executor/upstream.rs`'s `fetch_blocking_payload` and `fetch_stream_payload` — the
  two functions that actually build the outbound request to vLLM.
- **`types/io/`** — `input.rs` (inbound message/tool-call/tool-result shapes,
  `ResponsesInput`), `output.rs` (outbound output items: messages, function calls, web
  search/MCP calls, reasoning — plus the `ApplyDone` trait described below), `tools.rs`
  (the normalized `FunctionTool` and `ToolChoice`, distinct from tool *declarations*),
  `usage.rs` (token accounting structs). `ResponsesInput::model_input()` is the final
  model-visibility boundary used by `RequestPayload::to_upstream_request`: it removes
  orchestration-only `McpListTools` and `CompactionTrigger` input items. A persisted
  `Compaction` item is different: the latest checkpoint supersedes earlier model
  context and is converted into an assistant `output_text` summary, while canonical
  retained user messages and items after the checkpoint remain. This keeps rich
  continuation state available to orchestration without sending unsupported public
  item types to vLLM.
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

#### Output-to-input conversion for continuation rounds

`types/io/output.rs::OutputItem::to_input_item` is the single semantic conversion
from a response output item to the input item seen by a later inference round. Code
that continues a response must use this method; it must not serialize an `OutputItem`
as input or introduce another match over `OutputItem` variants.

```text
current round Vec<OutputItem>
        │
        ▼
filter_map(OutputItem::to_input_item)   ← the conversion policy
        │
        ▼
ResponsesInput                         ← append only
        │
        ▼
ResponsesInput::model_input()          ← final upstream-visibility filter
```

There are two adapters around that one policy:

- `executor/gateway.rs::append_output_items_to_input` calls
  `OutputItem::to_input_item` and appends the result to the current request before the
  next round. Its handling of `ResponsesInput::Text` versus `ResponsesInput::Items` is
  container mutation, not a second conversion policy.
- `storage/types/item.rs::InOutItem::into_input_items` uses the same method when
  rebuilding continuation input from persisted history. Already stored `InputItem`s
  pass through unchanged.

Messages, reasoning, function calls, custom calls, tool-search calls, compaction
items, and MCP list metadata have explicit continuation representations. Public
`web_search_call` and `mcp_call` output items return `None`: gateway execution records
the canonical model-facing function call and its `function_call_output` separately,
so converting the public projection would duplicate the call or lose result details.
Gateway tool results are already `InputItem`s and are appended through
`append_tool_outputs`; they do not need an output-to-input conversion.

### `events/` — parsing upstream SSE, and how to add a new event type

This module normalizes raw upstream SSE lines into typed frames, decoupled from the
executor so the accumulator doesn't do inline JSON parsing.

- **`types.rs`** — `SSEEventType` (the wire event's `type`, covering both OpenAI's and
  vLLM's naming, e.g. `response.done` vs `response.completed`), `EventPayload` (the
  typed, extracted payload — falls back to `Raw(Value)` for events not deeply parsed
  yet), `WireEvent` (the raw pass-through shape, used for re-serialization),
  `EventFrame { event_type, payload, wire }` (the normalized output), `SSEItemType`
  (output-item kind: reasoning, function call, MCP call, etc.).
- **`sse.rs`** — `SseLine` and `ClassifiedSseLine`. `SseLine::parse` performs only
  field-level SSE classification (`Data`, `Done`, or `Ignore`) and accepts the optional
  space after `data:`. Its redacted `Debug` implementation reports only payload size.
- **`normalize.rs`** — `normalize_sse_data_checked` parses one classified data payload
  into an `EventFrame` while preserving invalid-output-index errors for the ingestion
  policy. `extract_payload` dispatches to small per-event `extract_*` helpers. The
  public `normalize_sse_line` remains a convenience adapter for callers that do not
  need policy-aware errors.

**To add support for a new SSE event**, the touch points are, in order:
1. `events/types.rs` — add the `SSEEventType` variant, its wire-string mapping both
   directions, and (if it carries structured data) an `EventPayload` variant.
2. `events/normalize.rs` — extend `extract_payload` and add an `extract_*` helper if
   the payload needs real parsing (otherwise it can fall through to `Raw`).
3. Extend `executor/accumulator/`'s typed transition matches. Streaming callers enter
   through `RoundIngestion::push`; do not create a caller-specific validator or folding
   path.
4. If the event represents a client-executed function shape, extend the corresponding
   translator under `executor/translate/`. If it is gateway-synthesized, construct the
   typed `EventFrame` in `executor/gateway.rs`; `pipeline/delivery.rs` owns client relay.

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
  and builds the enriched `RequestContext`. Rehydration retains internal
  `InputItem::McpListTools` records so `ToolRegistry` can suppress repeated MCP
  discovery lifecycle output. After stored history and the new request input are
  combined, `pending_calls.rs` validates the complete continuation's function/custom
  call sequence. Every call and call output must have a non-empty `call_id`; call IDs
  must be unique across the sequence; and each output must resolve exactly one
  currently pending call of the same item kind. An output without a pending call, a
  second output for an already resolved call, or a function/custom kind mismatch is an
  invalid request rather than evidence that the call was resolved. Valid unresolved
  calls remain ordered by their original emission, and the first unresolved
  client-executed call produces the existing missing-output error. Gateway-executed
  built-in tool calls are resolved and recorded within their originating round, so
  they do not remain pending at this boundary.
- **`upstream.rs`** — the narrow adapter between inference transport and the pipeline.
  It builds `UpstreamRequest`s, snapshots registry classification facts into an owned
  `TranslationContext`, charges the request-wide response budget, and passes each live
  JSON or SSE body to `AgentPipeline`.
- **`inference.rs`** — `call_inference()`: the raw HTTP/SSE transport to vLLM. No
  parsing beyond splitting `data: ...` lines and stopping at `[DONE]`.
- **`pipeline.rs`, `pipeline/`** — `AgentPipeline`, the request-owned entry point for
  both response body formats. `RoundIngestion` owns the synchronous per-round semantic
  core; `StreamDelivery` owns awaited, ordered client delivery across rounds.
- **`engine.rs`** — the top-level orchestrator: `ExecuteRequest`/`execute()`,
  `create_conversation()`, and `EngineOrchestration`, which owns the request-scoped
  `ToolRegistry`, response budget, and mutable `AgentPipeline` while running the
  multi-round loop. Its local `classify_round`/`LoopDecision` decides whether to loop
  again, finish, hand back to the client, or return an incomplete response (capped at
  `MAX_GATEWAY_TOOL_ROUNDS = 10`). It accumulates output and token usage across rounds,
  changes continuation `tool_choice` to `auto`, and persists gateway calls plus their
  outputs as model-facing `InputItem`s. Also home to `run_compaction_trigger`,
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

#### Responses pipeline and ownership boundaries

RFC [#241](https://github.com/vllm-project/agentic-api/issues/241) and issue
[#243](https://github.com/vllm-project/agentic-api/issues/243) established one path for
JSON and live SSE response bodies:

```text
inference.rs
  ├─complete JSON───────────────────────┐
  └─framed SSE lines─▶ SseLine::parse───┤
                                        ▼
                                  AgentPipeline
                         │
                         ▼
                   RoundIngestion
                         │
                         ▼
              ResponseAccumulator
              (typed response state)
                         │ validated frame + typed call
                         ▼
              TranslationDispatcher
                (per-tool translators)
                         │ translated SSE frames
                         ▼
                  StreamDelivery
                         │
                         ▼
              GatewayStreamAccumulator
                         │
                         ▼
                       client

EngineOrchestration surrounds the pipeline:
inference rounds → gateway execution → terminal policy → persistence
```

| Stage | Responsibility |
| --- | --- |
| Inference transport (`inference.rs`) | HTTP I/O, byte chunks, SSE framing, timeouts, and `[DONE]` detection. |
| Event parsing (`events/`) | Classify one SSE line and normalize one data payload into an `EventFrame`. |
| Request pipeline (`pipeline.rs`) | Hold one request context, tool-search state, cross-round delivery state, and the JSON/SSE body entry points. |
| Round ingestion (`pipeline/ingest.rs`) | Process one body with one `ResponseAccumulator`, one `TranslationDispatcher`, and final response normalization. |
| Stream delivery (`pipeline/delivery.rs`) | Provide awaited sender delivery, gateway-event deferral and release, response IDs, and cross-round stream accumulation. |
| Orchestration (`engine.rs`) | Own the request-scoped registry and response budget, round decisions, gateway execution, terminal policy, and persistence. |

`AgentPipeline` lives for the complete public response. `EngineOrchestration` creates
one registry and one response budget around it, then asks `upstream.rs` to run each
inference body through `run_with_json_body` or live `run_with_stream_body`. A new
`RoundIngestion` is created for every body and consumed by finalization, while
`StreamDelivery` and `GatewayStreamAccumulator` survive across inference rounds.

The live runner polls one framed line, performs synchronous ingestion and translation,
then awaits delivery before polling the next line. This propagates bounded sender
backpressure to the upstream body. Ingestion remains inline; moving it to a worker is
the benchmark decision tracked by
[#245](https://github.com/vllm-project/agentic-api/issues/245).

The boundary contract is one owner and one path per concern:

- Strict and lenient validation use the same typed accumulator transitions; the policy
  selects validation/disposition rules and end-of-stream behavior.
- Output-item lifecycle state is scoped to one inference round and keyed by validated
  `output_index`. Item ID and kind must match on later events; active and completed
  slots remain distinct so index reuse and duplicate completion are rejected.
- Translation consumes validated `EventFrame`s plus the accumulator's typed function
  call view and produces the public wire lifecycle for client-owned tools.
- Delivery consumes translated frames and provides ordered, awaited client emission
  across inference rounds.

#### `accumulator/` — typed response assembly

`ResponseAccumulator` owns validation, response lifecycle, typed output slots, delta
folding, terminal error/incomplete state, usage, and final `ResponsePayload` assembly.
`slot.rs` contains the typed active/completed slot model and `json.rs` contains strict
JSON response-shape validation. Both JSON and SSE ultimately use the same finalization
state.

Output items are constructed through their `TryFrom<&EventPayload>` implementations in
`types/io/output.rs`. Active slots fold deltas in place and use the type's `ApplyDone`
implementation when its completion event arrives. Finalization promotes each completed
typed item once and preserves validated output-index order.

The pipeline's streaming entry is `process_line(ClassifiedSseLine)`. It normalizes and
validates a data line, applies the event to the slot keyed by its validated output
index, and returns at most one validated `EventFrame` for translation. For function
events, `accumulated_function_call(output_index)` exposes a borrowed typed call and its
folded arguments. `finish` applies SSE end-of-stream policy; `finalize` preserves the
status loaded from a complete JSON body.

When adding an output-item kind, extend its typed construction and completion logic in
`types/io/output.rs`, the slot variants in `accumulator/slot.rs`, and the exhaustive
transition and finalization matches in `accumulator/mod.rs`.

#### `translate/` — tool-specific public-shape translation

`TranslationDispatcher` is a synchronous inline dispatcher. It owns per-call
translation state, classifies each validated function call from an owned
`TranslationContext`, and routes client-executed tools to their specific
`ToolTranslator` implementation:

- `FunctionHandler` → `FunctionTranslator`
- `CustomHandler` → `CustomTranslator`
- `ShellHandler` → `ShellTranslator`
- `CodexNamespaceHandler` → `CodexNamespaceTranslator`
- `ToolSearchHandler` → `ToolSearchTranslator`

This is the reverse side of upstream canonicalization. The model emits the canonical
`function_call` shape. For a client-owned tool, its associated translator restores the
public Responses wire item and its SSE lifecycle (`output_item.added`, type-specific
delta/done events, and `output_item.done`). The ordinary function translator preserves
the function-call shape; custom, shell, namespace, and tool-search translators restore their
specific public forms.

Shell functions are restored to `shell_call` for client execution by default.
When an application explicitly registers a shell executor, the dispatcher suppresses
those internal function frames using the registry's resolved ownership snapshot;
the existing gateway event plan emits the shell call's added/done lifecycle.

The private `HasTranslator` association lives in the executor so tool handlers do not
depend on SSE or executor types. `TranslationContext` is an owned snapshot of the
registry facts and request metadata needed for classification and restoration. It
provides each round's tool classification and owns final restoration of public tool
declarations, custom/namespace shapes, tool choice, and tool-search metadata.

Gateway-executed types (`Mcp`, `WebSearch`, `FileSearch`, and `CodeInterpreter`) are
classified as gateway calls by the dispatcher, which suppresses their canonical
upstream function-call lifecycle. `gateway.rs` owns their execution result mapping and
synthesizes their public item lifecycle because the gateway knows when execution
starts, completes, or fails. `GatewayStreamAccumulator` then assigns cross-round
sequence numbers, rebases output indexes, and deduplicates response start events.
Events that arrive before a function name is known are buffered with a 256 KiB total
byte limit and replayed when the call resolves.

#### `gateway_accumulator.rs` and `pipeline/delivery.rs` — continuous client SSE

`StreamDelivery`, owned by `AgentPipeline`, is the only path from translated upstream
frames to the client sender. It withholds terminal upstream lifecycle events for the
engine, defers frames at and after the first hidden gateway-call index, and later
releases them in output order around synthesized gateway events. Deferred serialized
frames have a 256 KiB byte limit.

Its `GatewayStreamAccumulator` carries only cross-round presentation state: monotonic
`sequence_number`s, public `output_index` rebasing, and deduplication of response start
events. It holds no `OutputItem` assembly state. Sending is awaited before ingestion
continues, so sender closure or delivery failure propagates through the live pipeline.

#### `gateway.rs` — the tool-loop's building blocks

The round-by-round loop belongs to `engine.rs::EngineOrchestration`. `gateway.rs`
supplies the planning, execution, projection, and continuation helpers it calls each
round:
- `GatewayScheduler::plan` creates one slot per gateway-owned function call. Each slot
  owns the original item index, public output index, typed `GatewayBinding`, and
  lifecycle projection; a missing executor is represented by an explicit slot rather
  than omitted from a parallel vector. `GatewayScheduler::execute_with_budget` then returns one
  ordered `GatewayCallResult` per slot. `futures::future::join_all` polls all planned
  calls, while a `tokio::sync::Semaphore` created by each `execute_with_budget` invocation limits
  active tool executions in that round using `tools.max_concurrent_gateway_calls`
  (default `5`, configurable through `AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS`). This
  nonzero setting is carried by the owning `ExecutionContext` into each scheduler;
  the permits are local to the round, not a process-wide or cross-request limit.
  Completion may occur out of order, but `join_all` preserves model call order in
  the collected results. The permit limit bounds execution, not the number of
  planned call futures waiting for permits.
- A normalized `web_search` function call may batch at most five queries. The JSON
  Schema advertises the ceiling and the handler enforces it again because normalized
  web search currently uses non-strict arguments. Provider searches acquire a shared
  handler semaphore initialized from `tools.max_concurrent_gateway_calls`, preventing
  batched calls from multiplying the configured outbound concurrency. Results remain
  collected in query order for the public `web_search_call.action.queries` projection.
- Each bound call first acquires its optional same-tool exclusion permit, then a
  round execution permit, then a materialization permit shared by cloned scheduler
  policies (also used by MCP discovery, with a limit of 16). Waiting for same-tool
  exclusion does not consume a round permit; waiting for materialization does. The
  independent 60-second timeout wraps `GatewayBinding::execute` only after all
  permits are acquired. These permit waits do not count toward it, so it is not a
  deadline for the entire round or total call latency. Timeout, execution, and tool-config
  failures become failed tool outputs that can be fed back to the model instead of
  failing the whole response. A tool registered as gateway-owned without an
  implementation (currently file search/code interpreter) likewise produces an error
  tool result.
- Parallel safety is a per-handler contract. `GatewayExecutor::supports_parallel_execution`
  defaults to `false`; registration turns that into a `GatewayBinding::self_exclusion`
  semaphore. The semaphore serializes only simultaneous calls to the **same
  model-visible tool name**. It never blocks different tools from running concurrently.
  MCP and web search opt into same-tool parallel execution.
- Each scheduler slot retains its `GatewayEventPlan`; `emit_gateway_start_events` and
  `emit_gateway_completed_events` synthesize the OpenAI lifecycle for gateway-executed
  web search/MCP calls from those same slots. The ordinary path emits all planned start
  events, executes the round concurrently, then emits ordered completed/failed events.
- Streaming may receive client-visible output interleaved with gateway calls. In that
  case `engine.rs::execute_and_emit_ordered_output_calls` temporarily groups deferred
  upstream frames by `output_index`, executes the same `GatewayScheduler` concurrently,
  and then interleaves synthetic gateway lifecycle events with released upstream
  frames in original output order. Concurrency and wire ordering are therefore
  separate concerns.
- `public_output_items` is the public projection: custom function calls become
  `custom_tool_call`; gateway-owned internal function calls become their handler's
  `web_search_call`/`mcp_call` output; client-owned function calls remain function
  calls. The original gateway function calls and `function_call_output` results are
  retained separately for continuation persistence.

The round decision remains in `engine.rs`, after gateway execution:

| Decision | Condition and state transition |
|---|---|
| `RequiresClientAction` | At least one client-owned call exists. Any gateway calls from the mixed round have already executed; their internal calls/results are recorded before returning. |
| `Done` | No gateway result and no client-owned call remains. Finalize accumulated output and usage. |
| `Continue` | Gateway calls ran and round budget remains. Append the upstream output plus gateway results, set `tool_choice: auto`, and infer again. |
| `Incomplete` | Gateway calls ran on the tenth round. Record the final calls/results and return `status: incomplete` instead of leaving a dangling call. |

`parallel_tool_calls` is an upstream model-generation preference, not a gateway
scheduler switch. It is forwarded to vLLM for all supported declaration mixtures and
defaults to `false` when omitted. Whatever calls the model emits are executed under
the per-round execution permit limit and each handler's same-tool safety policy.

#### `messages_context.rs` / `messages_loop.rs` / `messages_request.rs` / `messages_stream.rs`

A **parallel, independent implementation** of the same shape of loop for the Anthropic
Messages API. `messages_stream.rs`'s own header comment describes it as "structurally
the Anthropic-native analogue of `GatewayStreamAccumulator`, kept deliberately parallel
for a future consolidation" — it never touches `RequestPayload`/`ResponsePayload`/
`AgentPipeline`/`ResponseAccumulator`/`GatewayStreamAccumulator`/`TranslationDispatcher`, operating
directly on Anthropic-shaped JSON. The two loops share only the protocol-neutral
pieces: `ToolRegistry::dispatch` and `types::messages::tool_seam`. The round/timeout
constants (`MAX_GATEWAY_TOOL_ROUNDS`, `GATEWAY_TOOL_TIMEOUT`) are duplicated and
manually kept in sync with the Responses-side ones rather than shared — a known seam,
not an oversight, per the future-consolidation note.

Both loops take a `MessagesRequestContext` (`messages_context.rs`), the per-request
type that replaced a bare `serde_json::Value` at that boundary. It holds two views of
one request: a typed `MessagesRequest` for reading `tools`/`stream`/`model`, and the
raw JSON body that is actually forwarded upstream. The raw body is deliberately *not*
re-serialized from the typed view — `ContentBlock` catches unmodeled block types in
`#[serde(other)] Unknown` and models only the fields the gateway reads, so a typed
round-trip would drop `cache_control` and `is_error` and collapse `image`/
`redacted_thinking` into `{"type":"unknown"}`. The context owns every mutation the
loops make to that body (`force_stream`, `append_round`) and the native web-search
budget, so the two views cannot drift apart uncontrolled; `messages` and `system` are
reachable only through the raw body, never the typed view.

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
  `OutputItem`), and `StorageError`. `InOutItem::into_input_items` turns a full
  history into the `Vec<InputItem>` used for continuation processing: stored
  `InputItem`s pass through, while stored `OutputItem`s go through
  `OutputItem::to_input_item()`. Messages, reasoning, function/custom calls,
  compaction checkpoints, and MCP list-tools records are retained. Public
  `web_search_call` and `mcp_call` outputs are deliberately omitted because their
  model-facing function calls and results are already persisted as input items;
  reconstructing them here would duplicate and lose information from that canonical
  pair.

  This conversion is **not** the model visibility boundary. The resulting enriched
  history still contains `InputItem::Compaction`, `InputItem::CompactionTrigger`, and
  `InputItem::McpListTools` for executor/registry decisions. Immediately before an
  upstream request, `RequestPayload::to_upstream_request` calls
  `ResponsesInput::model_input()`: the latest compaction checkpoint is converted to an
  assistant summary and supersedes older context, while compaction triggers and MCP
  list-tools records are removed. In particular, MCP list-tools remains available long
  enough for the registry to remember which server labels have already been listed,
  but it is never serialized to vLLM.
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

Every supported tool, whether client-owned or gateway-owned, implements `ToolHandler`.
That trait is the authority for validating a public declaration and normalizing it to
the fixed `FunctionTool` format understood by the upstream model. The outbound path is:

```text
public ResponsesTool declaration
        │
        ▼
RequestPayload::to_upstream_request
        │
        ├─▶ ResponsesTool::validate ──────────▶ ToolHandler::validate
        └─▶ ResponsesTool::to_function_tools ─▶ ToolHandler::normalize
                                                     │
                                                     ▼
                                      canonical UpstreamTool::Function
```

`RequestPayload::to_upstream_request` is the only request-level seam that prepares
tools for vLLM. New callers must use it rather than rebuilding function schemas or
normalizing declarations in the executor. Declared placeholders that are not yet
supported, currently file search and code interpreter, produce no upstream function
declaration until they have a complete handler and execution path.

| Component | Responsibility |
| --- | --- |
| `ToolHandler` | Validate one supported public tool declaration and normalize it into one or more canonical model-visible `FunctionTool`s. |
| `ToolRegistry` | Provide the request's read-only name lookup for all available tools, including tool type, client/gateway ownership, server label, and any typed gateway binding. |
| Client `ToolTranslator` | Convert a validated canonical function call back to that client-owned tool's public output shape and SSE lifecycle. |
| `GatewayExecutor` and `gateway.rs` | Execute gateway-owned calls and map their start, completion, failure, result, and public output lifecycle. |
| `GatewayStreamAccumulator` | Project gateway and upstream lifecycle frames into one continuous, correctly indexed and sequenced client stream. |

- **`normalize.rs`** — the `impl ResponsesTool` block with `validate()` and
  `to_function_tools()`. These are the declaration-level validation and normalization
  entry points used by `RequestPayload::to_upstream_request`. Each supported variant's
  policy belongs to its corresponding `ToolHandler`: `FunctionHandler`,
  `ToolSearchHandler`, `McpHandler`, `WebSearchHandler`, `CodexNamespaceHandler`, or
  `CustomHandler`. Web search's fixed canonical builder is shared with
  `WebSearchHandler::normalize`; it remains one schema even though it has no
  per-declaration normalization state. The method name is plural because namespace and
  MCP declarations may expand to several model-visible function tools.
  `FileSearch`/`CodeInterpreter` remain unsupported placeholders and normalize to
  nothing.
- **`handler.rs`** — the two traits every tool type reasons about:
  ```rust
  pub trait ToolHandler: Send + Sync {
      type ToolParams: Send + Sync;

      fn tool_type(&self) -> ToolType;
      fn validate(&self, params: &Self::ToolParams) -> Result<(), ToolError>;
      fn normalize(&self, params: &Self::ToolParams) -> Vec<FunctionTool>;
  }

  pub trait GatewayExecutor: ToolHandler + 'static {
      type ExecutionParams: Clone + Send + Sync + 'static;

      fn execute(
          &self,
          call_id: &str,
          tool_name: &str,
          arguments: &str,
          params: &Self::ExecutionParams,
      )
          -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>>;
      fn supports_parallel_execution(&self) -> bool;
      fn plan_gateway_events(
          &self,
          call: &FunctionToolCall,
          params: &Self::ExecutionParams,
      ) -> GatewayToolEventPlan;
      fn public_output(
          &self,
          call: &FunctionToolCall,
          output: &ToolOutput,
          status: GatewayCallStatus,
          params: &Self::ExecutionParams,
      ) -> Option<OutputItem>;
  }
  ```
  `GatewayExecutor` requires `ToolHandler`: every executable gateway handler supports
  typed validation and normalization, but not every `ToolHandler` is gateway-executable.
  This inheritance is the ownership invariant: supported client and gateway tools
  share the same declaration contract before execution ownership matters.
  `ToolParams` describes the public declaration; `ExecutionParams` describes one
  model-visible executable entry. They intentionally differ for MCP: an
  `McpToolParam` declares a server, while an `McpDiscoveredToolParam` identifies one
  tool returned by that server. Gateway-owned registry types may also lack an executor
  entirely. The trait owns three runtime hooks: `supports_parallel_execution()`
  controls same-tool self-exclusion, `plan_gateway_events()` creates the typed public
  lifecycle projection, and `public_output()` shapes the completed/failed
  client-visible item.
  - **Client-owned** tools implement `ToolHandler` and have a translator association:
    see `function.rs` (`FunctionHandler`), `custom.rs` (`CustomHandler`), `codex.rs`
    (`CodexNamespaceHandler`), and `tool_search.rs` (`ToolSearchHandler`). Their calls
    are returned for the client to resolve; the gateway does not execute them.
  - **Gateway-owned / built-in** tools implement both traits: see `web_search.rs`
    (`WebSearchHandler`, backed by You.com) and `mcp/handler.rs` (`McpHandler`, backed
    by `mcp/client.rs`'s MCP protocol client and `mcp/pool.rs`'s connection pool). They
    have no client translator association because the gateway owns their execution and
    public lifecycle.
- **`ownership.rs`** — `ToolOwnership::Client` versus
  `ToolOwnership::Gateway(Option<GatewayBinding>)`. A `GatewayBinding` combines the
  resolved executor, its typed `ExecutionParams`, and the optional same-tool semaphore
  derived from its parallel-safety declaration. A generic adapter checks the
  executor/parameter pair at construction and erases only that valid bound pair for
  heterogeneous registry storage. The scheduler therefore never handles untyped JSON
  configuration or downcasts. Keeping ownership explicit avoids inferring execution
  policy from whether a handler happens to be present.
- **`registry.rs`** — `ToolRegistry`, a request-scoped map from model-visible tool name
  to `ToolEntry { tool_type, server_label, ownership }`. Its responsibility is to keep
  the read-only catalog of every available model-visible tool for the request,
  including declared client tools, namespace members, built-ins, and discovered MCP
  tools. A lookup answers which tool type a name identifies and whether its ownership
  is `Client` or `Gateway`; a gateway entry may also carry its typed
  `GatewayBinding`. Its constructor,
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
  method the Messages loop uses; the Responses `GatewayScheduler` resolves the same
  binding into one call plan so execution, self-exclusion, item position, and lifecycle
  hooks cannot drift apart. After construction, consumers query this catalog for
  classification, ownership, and gateway bindings. `upstream.rs` snapshots its
  classification facts into a per-round `TranslationContext`, which translators use
  when restoring client-owned tool calls.

  MCP discovery history is also request-scoped registry state:
  `mcp_list_tools_items: HashMap<String, Vec<McpListTools>>` groups records by
  `server_label`. Registry construction puts the current discovery item first;
  rehydration appends prior `InputItem::McpListTools` records only for labels already
  present in that map. `mcp_list_tool_items()` exposes entries whose vector still has
  exactly one element—the current item with no history—to both blocking output
  assembly and streaming lifecycle emission. Streaming clears the map after the first
  inference round. Consequently a server's list lifecycle is emitted only when no
  prior list record exists and never repeats across rounds.
- **`executors.rs`** — `GatewayExecutors`, a shared registry built once at startup and
  reused across requests, specifically for gateway tools that need **lazy, per-request
  connection setup**: MCP servers (connects and caches `McpClient`s keyed by server
  URL, falling back to connecting a fresh request-declared server) and the shared
  `WebSearchHandler`. It also has an optional, application-provided `ShellExecutor`
  slot. `GatewayExecutorRegistration::Shell` is an explicit execution grant; an
  unregistered shell declaration remains client-executed. `ShellExecutor` accepts
  a typed call with bounded action limits and cancellation and returns typed command
  outputs. The adapter binds into the existing gateway scheduler, not a second tool loop.
  Client-owned
  tools (`function`, `custom`, `namespace`) never touch this file; their registry
  entries are inserted with `ToolOwnership::Client` and no `GatewayExecutors`
  involvement.

Shell item history is preserved publicly in storage. At the inference boundary,
`ShellHandler::model_input` lowers shell calls and outputs into matching function
history, just as declarations and explicit shell selectors are normalized. For an
opt-in gateway executor, storage additionally retains the canonical internal function
call/output pair; rehydration omits that pair's public shell-call projection to avoid
replaying the invocation twice. Client-executed shell history is not omitted.

**To add a new tool type:**
1. Implement `ToolHandler`, including its typed `ToolParams`, for it.
2. If it's gateway-executed, also declare typed `GatewayExecutor::ExecutionParams`
   and implement `execute` — see `web_search.rs`/`mcp/handler.rs`.
3. Wire it into `tool/normalize.rs`'s `validate`/`to_function_tools` match arms.
4. Wire it into `tool/registry.rs`'s `build_with_handlers` (an `insert_*_entry` call).
5. For a client-executed function shape, add its `ToolTranslator` and associate the
   handler in `executor/translate/client.rs`. Gateway-executed tools do not receive a
   translator association; their public events come from gateway lifecycle plans.
6. If it needs lazy per-request connection setup, add a slot to `GatewayExecutors` in
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
| Support a new upstream SSE event | `events/types.rs` → `events/normalize.rs` → `executor/accumulator/` → `executor/translate/` when the event needs public tool-shape translation |
| Add a new tool type | `tool/handler.rs` impl(s) → `tool/normalize.rs` → `tool/registry.rs` → `executor/translate/client.rs` for client-executed function shapes → `tool/executors.rs` for lazy gateway setup |
| Change gateway-round concurrency or lifecycle ordering | `executor/gateway.rs` (`GatewayScheduler`/event plans) + `executor/engine.rs` (round decision/ordered streaming) + `tool/ownership.rs` (typed binding and same-tool safety) |
| Change client streaming order, buffering, or backpressure | `executor/pipeline/delivery.rs`; keep parsing in `events/` and response assembly in `executor/accumulator/` |
| Move streaming ingestion to a worker | Benchmark the equivalent inline and worker paths under [#245](https://github.com/vllm-project/agentic-api/issues/245) before changing executor placement |
| Feed response output into the next inference round | `types/io/output.rs::OutputItem::to_input_item`; use `executor/gateway.rs::append_output_items_to_input` only to append those converted items |
| Change continuation history visibility | `storage/types/item.rs::into_input_items` → `types/io/output.rs::to_input_item` (preservation) → `types/io/input.rs::model_input` (upstream visibility) |
| Add a CRUD operation beyond persist/rehydrate | `executor/modes/conversation.rs` or `modes/response.rs`, backed by `storage/conversation.rs` / `storage/response.rs` |
| Change how output items are assembled from a stream | `executor/accumulator/` — extend the typed slot and transition matches through the existing `RoundIngestion` path |
| Add a new Responses/Messages wire field | `types/io/` or `types/messages/` — shape only, no behavior |

## Further reading

- [AGENTS.md](AGENTS.md) — module boundaries, lint/format rules, commit and PR conventions
- [TERMINOLOGY.md](TERMINOLOGY.md) — normative vocabulary for API/state/tool/streaming concepts
- [ROADMAP.md](ROADMAP.md) — project direction and near-term focus
- [docs/adr/](docs/adr/) — architecture decision records
- [docs/design/](docs/design/) — as-built design docs (tool framework, core public API, MCP integration, Codex integration)
