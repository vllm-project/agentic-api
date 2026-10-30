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
persisted as one response. The single-agent path is:

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
continuous SSE lifecycle. With `multi_agent.enabled: true`, a response-level coordinator
schedules separate agent contexts through the same inference, tool, and delivery
boundaries; see [HTTP multi-agent execution](#http-multi-agent-execution).

## `agentic-server` — the transport layer

### Two binaries sharing one library

The crate produces a library plus two independent binaries. `src/lib.rs` exports
`agentic_cli`, `agentic_harness`, `agentic_output`, `agentic_process`, `app`, `auth`,
`handler`, `model_capabilities`, and `rate_limit`. On top of that:

| Binary | Entry point | Uses |
|---|---|---|
| `agentic-server` (the gateway) | `src/main.rs` | `app`, `auth`, `handler`, `model_capabilities`, `rate_limit`, plus binary-private `server.rs`, `config_file.rs`, and `server_settings.rs` |
| `agentic` (the CLI launcher) | `src/bin/agentic.rs` | `agentic_cli`, `agentic_harness`, `agentic_output`, `agentic_process`, `model_capabilities` |

These are two unrelated concerns bundled in one crate. If you're working on request
handling, ignore `agentic_cli*`/`agentic_harness.rs`/`agentic_output.rs`/
`agentic_process.rs` entirely — they're the launcher that spawns the gateway binary and
a coding harness (Codex or Claude Code) as subprocesses for local, single-command use
(`agentic serve <model>`), and never touch the request path.

`model_capabilities.rs` is the one deliberate exception: it is the shared contract both
sides speak. The gateway resolves each model's `InputModalities` and reasoning effort levels
there and serves them in the Codex catalog; the launcher parses that same catalog back through
`CodexCatalogCapabilities` before writing an isolated Codex home. Keeping one definition is
what stops the HTTP catalog and a launcher catalog from disagreeing about image support.

### `app.rs`, `server.rs`, `main.rs`

- **`app.rs`** (library) builds the router: `AppState` (the per-request-shared state:
  `exec_ctx: Arc<ExecutionContext>`, proxy state, readiness/websocket trackers, config)
  and `build_router_with_auth(state, server_config, authenticator)`, which wires every
  route and layers [request admission](#request-admission-authentication-then-rate-limits)
  onto the protected ones: optional OIDC auth (`auth::require_oidc`), then optional rate
  limiting (`rate_limit::enforce_rate_limit`).
- **`server.rs`** (binary-private) owns process lifecycle: `build_state` (constructs
  `ExecutionContext::from_config`, i.e. where the DB pool actually gets created),
  `serve_gateway`/`serve_gateway_until_signal` (bind, serve, graceful shutdown with a
  bounded drain), and `run`/`run_with_llm` (standalone mode, optionally spawning a vLLM
  subprocess). It serves with `into_make_service_with_connect_info::<SocketAddr>()`, so
  middleware can read each connection's peer address.
- **`main.rs`** (binary-private) is the `clap` CLI front end: parses config from
  flags/env/`config.toml`, then calls `server::run` or `server::run_with_llm`.
- **`server_settings.rs`** (binary-private) resolves the gateway admission settings from
  the `[server]` table: the request body size ceiling (`resolve_max_request_body_size`)
  and the per-caller rate limit (`resolve_rate_limit`, which returns `None` when no rate
  is set and rejects a burst without a rate). Each setting resolves as CLI argument >
  environment variable > configuration file > default, and an explicit CLI argument
  skips the lower sources, so a malformed inherited environment value cannot block
  startup. `generated_server_file_config` records the same environment variables when
  `main.rs` generates a configuration file.

### HTTP handlers (`handler/http/`)

| Route | Handler | File |
|---|---|---|
| `POST /v1/responses` | `responses` | `handler/http/responses.rs` |
| `GET /v1/responses/{response_id}` | `retrieve_response` | `handler/http/responses.rs` |
| `POST /v1/responses/compact` | `compact_response` | `handler/http/responses.rs` |
| `POST /v1/conversations` | `create_conversation` | `handler/http/conversations.rs` |
| `GET/POST/DELETE /v1/conversations/{id}` | Conversation CRUD | `handler/http/conversations.rs` |
| `POST/GET /v1/conversations/{id}/items` | Batch append and ordered listing | `handler/http/conversation_items.rs` |
| `GET/DELETE /v1/conversations/{id}/items/{item_id}` | Item retrieval and removal | `handler/http/conversation_items.rs` |
| `POST /v1/messages` | `messages` | `handler/http/messages.rs` |
| `POST /v1/messages/count_tokens` | `count_tokens` | `handler/http/messages.rs` |
| `GET /v1/models` | `models` | `handler/http/models.rs` |
| `GET /v1/models/{model}` | `retrieve_model` | `handler/http/models.rs` |
| `POST /v1/chat/completions` · `POST /v1/completions` | `chat_completions` · `completions` | `handler/http/chat_completions.rs` |
| `POST /v1/embeddings` | `embeddings` | `handler/http/embeddings.rs` |
| `POST /v1/audio/transcriptions` · `POST /v1/audio/translations` | `transcriptions` · `translations` | `handler/http/audio.rs` |
| `GET /health` | `health` | `handler/http/models.rs` |
| `GET /ready` | `ready` | `handler/http/models.rs` |

The HTTP Responses handler parses a typed `RequestPayload` and always builds an
`ExecuteRequest` against `state.exec_ctx`. Both `store: true` and `store: false`
requests use input normalization, tool normalization, validation, and the tool loop.
Unmodeled top-level provider parameters, such as `chat_template_kwargs` and `top_k`,
are retained in `RequestPayload::extra` and borrowed by `UpstreamRequest` for forwarding
on each inference round. Known fields keep their typed validation and conversion;
gateway state controls and WebSocket event metadata are not provider extensions.
With `store: false` and no state IDs, execution does not rehydrate stored history or
persist the turn. Clients supply the complete item history in `input`, including
prior output items and tool call outputs. `previous_response_id` still loads an
available stored parent; `store: false` prevents the new response from being stored.
An explicit `conversation` loads its stored history, but `store: false` does not
append the new input/output items or store the response.

`UpstreamRequest` carries `store:false` whenever gateway storage is disabled.
Typed admission still rejects unsupported body shapes. Provider reasoning context
and opaque-state compatibility require coordinated backend work, tracked in
[#444](https://github.com/vllm-project/agentic-api/issues/444).

HTTP execution supplies request-scoped headers through `with_upstream_headers`, using
the proxy's existing end-to-end filter and authentication fallback. These headers
travel through every inference and compaction round, without entering stored state.
`run_http` polls the same stream-owned producer until the first client frame, allowing
initial upstream failures to retain their HTTP status and processed response headers.
Later failures remain SSE events. Embedded `run` callers keep lazy stream startup.

The Messages handler selects `run_messages_loop`/`run_messages_stream` for requests
that need gateway execution. Other requests and pass-through endpoints forward to
the upstream through `agentic_core::proxy`.

### WebSocket transport (`handler/websocket/`)

`GET /v1/responses` upgrades to a WebSocket. Structurally this is not a one-shot
handler like the HTTP routes — `responses_ws_loop` is a long-lived session loop that
decodes typed create, injection, and steering messages and uses `ExecuteRequest::prepare_retained()`
to drive the same inference/tool loop and persistence APIs as HTTP. Requests with distinct
`stream_id` values run concurrently, while requests in the same lane remain FIFO;
requests without a `stream_id` share a default FIFO lane. The session admits at most
64 active or queued requests and 12 MiB of aggregate request data. WebSocket sessions
force `stream: true` and honor the requested `store` value. Because axum's built-in graceful shutdown
doesn't wait for upgraded connections, `AppState` carries a separate
`WebSocketTracker` so shutdown can drain in-flight sessions. `connection.rs` owns socket
reading, writing, and shutdown; `multiplexer.rs` owns bounded create admission and lane FIFO;
`control.rs` owns retained response routes and bounded task lifetimes. Its private
`control/injection.rs` handles live client tool outputs; `control/steering.rs` owns
accepted-batch lifecycle, pending outcomes, and successor handoff. Its private
`steering/admission.rs` decodes and resolves submissions, validates eligibility,
reserves capacity, and delivers acceptance or rejection against the same route state.
Lane advancement and automatic scheduling return `Result<(), WsError>`; successful idle scheduling is
`Ok(())`. The socket loop owns the disconnect decision and logs scheduling or task-join failures.
An automatic admission failure closes the unprepared target before its deferred cleanup runs.

Generating creates prepare the existing executor before starting a gated retained
owner. `PreparedRetainedResponse::start_gated()` allocates the response task;
the connection registers its route and relay before `ResponseRunOwner::activate()`
releases execution. The owner's `ResponseRunState` transitions from
`AwaitingRegistration` to `Running` to `Joined`. Ordinary core callers use
`PreparedRetainedResponse::start()` with `ResponseStartMode::Immediate`, which
creates no activation channel. Cancellation and joining remain separate operations.

`response.steer` targets a generating, single-agent response on the same
connection. The target supplies its lane; the steering envelope has no
`stream_id` or response settings. HTTP/SSE keeps ordinary Responses continuation
and exposes no steering operation. Multi-agent tool outputs use injection;
multi-agent steering is unsupported. Conversation-bound requests, compaction/context management,
unsupported client-call obligations, and non-generating prewarm targets are
outside the steering interface. Support is not inferred from a model-name prefix.

Validated `SteeringInput` wraps `ResponsesInput` and reuses its user messages and
text/image content types. Shared input types still represent files, but file execution is not supported
or qualified by this steering suite. `request.rs` checks original envelope/message fields
before shared decoding, while `SteeringInputEcho` retains bounded original JSON
for failed outcomes. This preserves rejected fields and explicit nulls that shared
message decoding can discard or normalize. Accepted input becomes ordinary user
`InputItem`s in the successor, not an output slot in the running response.
`ResponsesInput::into_canonical_items()` in `types/io/input/conversions.rs`
preserves submitted item variants; `normalized_model_input()` still runs at the
ordinary upstream request boundary.

The existing `SteeringTarget` owns `SteeringState` in `control.rs`. An optional
`SteeringBatch` groups its accepted queue, original-input echoes, and parent pin.
The state also owns an optional settings snapshot with a separate connection-level
12 MiB reservation, acquired even for unsteered targets. Periodic expiry and steering
admission release superseded snapshots only after observing a newer creation receipt
on the same lane; accepted batches keep theirs through commitment or failure. Settings
overflow disables steering for that target without cancelling the response. Closing
the state drops its settings reservation and transfers or drops the whole batch; terminal
paths do not clear its resources independently. Its admission
lifecycle is independent of the response task and terminal delivery:

| `SteeringPhase` | Meaning |
| --- | --- |
| `Running` | Parent execution is active; accepted updates wait for its safe completion boundary. |
| `Ready` | Parent execution has joined; continuation can be prepared when required client outputs are available. |
| `Reserved` | A validated successor owns the sealed batch; its publication decides commitment or failure. |
| `Closed` | Further admission is closed; retained input is released on commitment or returned through failed outcomes. |

Parent completion advances `Running` to `Ready`, so an eligible completed parent
can still accept steering. Fresh batch reservation advances `Ready` to `Reserved`;
publication or fenced failure closes it. Preparation checks the phase both before
and after its awaits. An automatic admission/preparation failure closes an
unreserved target before asynchronous cleanup can race another attempt. Delayed
completion callbacks cannot reopen a reserved or closed target. Route lookup also
checks for a newer published response in the same lane; another lane does not
supersede or consume the target's accepted input.

`SteeringQueue` in `executor/session/steering.rs`, re-exported by `session.rs`, owns
ordered accepted input and its byte reservations. A `ResponseCheckpointPin` retains the exact parent across failed
continuation attempts through the unified
`with_session(session, Option<&ResponseCheckpointPin>)`
API. Preparation copies canonical input without consuming admission; a consuming
`SteeringSuccessorReservation` checks the complete ordered batch again before
sealing it. A new admission during preparation invalidates that snapshot and
returns both owners for fresh preparation. Gated startup transfers the queue to
`SteeringSuccessorPublication`, which commits only with its exact sink's
`ResponseCreatedReceipt`, recorded after bounded enqueue and sequence commitment.
This proves publication to the relay queue, not peer receipt. Pre-creation failure
recovery requires cancelling and joining execution, then checking creation again.
`fail_before_creation()` releases the uncommitted canonical batch; the adapter
already owns the original inputs and IDs for failure delivery. Later failure after
creation cannot return or replay committed steering.

The retained sink's steering signal wakes `AgentPipeline` only at a safe item
boundary. The existing synchronous accumulator owns the completion proof described
below; the engine drains admitted gateway-executed tool batches before stopping
further inference. An intentional cut ends the parent as incomplete with reason
`steered`; natural completion can win. A completed client-executed call keeps the
ordinary completed parent and its required-output obligation.

The adapter uses the existing pending-call scanner to emit
`response.steer.pending` with function, custom, shell, and client tool-search output stubs. A matching same-lane
`response.create` supplies all required outputs without resending steering.
Pending stubs use the pinned parent's public tool declarations to distinguish custom and shell calls from their
normalized model-facing function representation. Namespaced functions retain their function obligation.
Rejected outputs preserve the queue and pinned parent for a corrected attempt;
partial rejected batches are not accumulated. Accepted updates are prepended in
order before the explicit create's input. That create uses its own settings and
defaults. Ordinary continuations that explicitly replace the tool list also select their own tool choice
or its default; a removed tool cannot retain the parent's forced selector. When no client output is required,
an automatic successor inherits the
immutable snapshot captured by `RequestPayload::continuation_settings()`, with
fresh execution budgets. The snapshot excludes canonical input and parent/conversation
routing. Active tool-search preparation restores this snapshot's tools from the public
effective catalog, including deferred definitions, rather than its private inference view.
Original configuration qualifies conversation and compaction restrictions
before those fields are removed.

Each target admits at most 32 steering inputs, with a 65-entry ordered outcome
queue and a shared 12 MiB connection budget for accepted raw input echoes and settings.
Settings are charged on first acceptance and refunded on successor publication or failure;
ordinary responses do not consume this budget. Unaccepted settings snapshots remain
bounded by the 128-route cap, inbound request ceiling, and route expiry. Only accepted
steering pins checkpoints beyond the session's ordinary latest-parent retention.
Overflow returns a routed `response.steer.failed` with `too_many_pending_steers`.
A separate 12 MiB allowance bounds failure echoes when the accepted-input budget is
full; outcome queue or failure-delivery exhaustion remains a transport overload.
`response.steer.accepted` acknowledges retained ownership, not application.
Typed accepted/pending/failed outcomes reuse `ResponseEventSink` and its sequence/flush barriers,
including after terminal delivery. Routes with accepted input remain retained;
unapplied input is disposed with the connection. Other lanes cannot consume it.
Superseded targets reject further steering rather than redirecting input.

WebSocket session admission allows multi-agent configuration with either `store` value,
including configuration inherited from a stored agent tree. The common multi-agent
validation still runs before and after restoring effective settings. Generating responses
register connection-local routes before their first event is relayed. Injection bypasses
create-lane FIFO and submits typed batches to the core `RunControl`; only the coordinator
validates call ownership and mutates agent histories.

`types/websocket.rs` owns the create/inject/steer client envelopes; create flattens the
shared `RequestPayload` and adds `generate` and `stream_id`. Injection accepts typed
function, shell, custom-tool, and discovery outputs. OpenAI duplex recordings establish
live function, shell, and discovery acceptance and preservation of custom outputs in
late rejections. Core validates output kind and call ownership. Live discovery reuses
the existing tool-search preparation and registry builder before the owning agent's next
round, preserving its inference-round budget.
Unknown and retained-completed targets return `response.inject.failed` with the documented
`response_not_found` and `response_already_completed` codes and unchanged typed input.
Malformed injection schema closes the connection after a generic validation error.
An already-resolved call instead returns `response.inject.failed` with `invalid_input`
and the submitted input, leaving the connection usable, as observed in the OpenAI capture.
Paired active recordings confirm atomic rejection of duplicate-within-batch and mixed
valid/unknown-call batches, successful valid-only resubmission, and live duplicate rejection.
Deterministic core and transport tests exercise the same decisions. A quiescent tree with pending client
calls completes and persists its checkpoint for continuation rather than waiting indefinitely
for socket input. Commands losing the finalization race return their input after commit.

The connection owns control and relay `JoinSet`s, at most 32 admitted controls with
12 MiB of aggregate serialized input, and 128 response routes. Completed routes expire
after 60 seconds or are evicted under capacity pressure; an admitted task pins its route
through acknowledgement relay. Control exhaustion closes the connection, with an overload
error when the bounded outbound queue can admit it. There is no overflow buffer.
`ResponseEventSink` retains the response's sequence owner and one-entry delivery queue
after execution. Accepted decisions enqueue acknowledgements through that same sink;
a typed flush barrier keeps terminal delivery ahead of the next create in its lane without
carrying dummy frame data or advancing the public sequence.
An independent socket writer drains the shared 64-entry queue with a ten-second send
deadline, so the reader never waits for a socket write. Failed acknowledgement delivery
is a transport failure, never an instruction to retry accepted input. Disconnect cancels
and joins response owners and admitted controls. Graceful shutdown drains ordinary active
creates and cancels live multi-agent runs, which could otherwise wait forever for new
client input after admission stops. Admitted acknowledgements retain their delivery leases.
Non-generating prewarm creates use local completion and apply continuation input through
the core's agent ownership validation before committing the updated tree, without inference.

OpenAI/gateway duplex recordings and deterministic tests cover injection workflows
and active edge cases. Steering has four categories recorded against both OpenAI and the gateway. The
[gateway WebSocket tests](crates/agentic-server/tests/responses_websocket_test.rs), including held built-in work,
required-output recovery, connection lifetime and successor publication failures, also compare the captured
causal contracts and replay recorded client workflows through real handlers with local model dependencies.
Those dependencies are explicitly synthetic: public duplex recordings do not contain upstream model HTTP
requests. A core executor companion replays captured interpreter work through the real tool supervisor with
a controlled provider. UUIDs, generated prose, token usage and model-dependent call counts are not parity keys.
Recorded gateway defects and probes that missed their prerequisites are classified separately; fixing the
gateway does not rewrite the original evidence. See the
[cassette recording guide](crates/agentic-server-core/tests/cassettes/README.md#mid-turn-steering-exploration).
Synthetic tests do not establish real-backend parity or load performance.
Full replay of independently recorded model and hosted-tool dependencies
and comparative resource measurements remain follow-up work; this implementation does
not advertise complete transport conformance.

Executor streams propagate downstream backpressure through a bounded event channel.
`[responses]` configures separate ceilings for upstream JSON bodies, upstream SSE
lines, retained response data, and client stream events. Wire ceilings default to
16 MiB each. Each request shares an 8 MiB retained-data budget across MCP discovery,
upstream rounds, and normalized gateway tool output. Ingestion charges logical retained
items rather than repeated SSE envelopes, so changing delta size does not change the
budget for the same output. These are logical limits, not total process memory bounds. MCP discovery participates in the same 16-permit
materialization window as gateway calls and is capped at 64 server declarations and
128 discovered tools per request.
The WebSocket transport queues only serialized, size-checked events, capped at the
configured `max_stream_event_bytes` including routing metadata, in its 64-entry outbound
queue. `handler/websocket/responses/event.rs` owns that envelope and its limit. The
executor receives the effective event ceiling after routing overhead and checks its
terminal event before persistence or session checkpoint publication. Local completion
validates both lifecycle events before persistence. A slow connection can retain up to
64 times the configured event ceiling in its outbound queue, in addition to executor
queues, parsed upstream payloads, and active response state. Authentication is
rechecked at request dispatch so queued work cannot start after identity expiry.
Errors are modeled by a dedicated `WsError` enum (`handler/websocket/error.rs`) rather
than reusing the HTTP JSON-error path, since some failure modes (a dead socket) must
not attempt to write a response.

### Opt-in core continuation sessions (`executor/session.rs`)

`session.rs` owns session-group admission and idle-slot lifecycle. Its private
`session/checkpoint.rs` owns checkpoints, pins, and continuation leases;
`session/steering.rs` owns accepted input, successor reservations, and publication.
The public types remain re-exported through `executor::session`.

Core callers can use `ExecuteRequest::with_session(session, pin)` or `rehydrate_in_session` to
retain response state without durable storage. The WebSocket multiplexer owns one
`ResponseSessionGroup` per connection and keeps one session per lane, including idle
lanes. No-session HTTP and split execution keep their existing behavior.

`pin: None` uses ordinary parent lookup. `Some(&ResponseCheckpointPin)` calls
`begin_pinned` with the exact owning session and `previous_response_id`, including
after a failed attempt has evicted the ordinary cache. Pinned attachment rejects
explicit conversation binding. `pin_response` shares the immutable latest checkpoint
and its existing byte reservation without occupying an execution slot. The pin
survives cache eviction but cannot reopen a closed session or retarget the parent.
Steering owns the pin through successor commitment or failure cleanup.

The connection retains at most 128 lanes (including the default lane), 32,768 items
and 16 MiB per checkpoint, and 32 MiB of aggregate serialized checkpoints. These are
retention ceilings, not measured process-memory bounds. New lanes receive an
immediate 429 once the lifetime lane limit is reached. Request-count and request-byte
overloads also return immediate 429 responses without mutating retained state, so
rejected work cannot invalidate an earlier accepted queued continuation. Ordinary
continuation validation errors execute in lane order and evict only a matching
referenced parent when routing is valid. Parent lookup happens when execution begins; accepted queues
do not reserve parent snapshots. Accepted steering separately retains its exact
parent pin and input across rejected matching continuations. Existing ordinary fork
and execution-failure eviction rules still apply. Disconnect aborts and joins
request tasks, waits for active leases to release pinned state, and drops the entire
connection group before the close handshake.

A `ResponseSession` owns one latest canonical checkpoint and one execution slot.
The executor pins a parent before inference and publishes completed or incomplete
state before exposing terminal completion. Failed continuations discard only a
referenced checkpoint owned by that session. Dropping the owner closes the session
and rejects late publication. Callers still cancel and join active work explicitly;
`wait_until_idle` waits for the execution lease to end, but does not cancel it.

`ResponseSessionGroup` allows independent serial members to find and pin each
other's latest checkpoints. Failed forks cannot evict the source member's state.
The group bounds lifetime member count, each checkpoint's items and serialized
bytes, and aggregate retained bytes. Shared parent references count once; replaced
parents still pinned by active work and prepared checkpoints awaiting persistence
remain charged until their last reference is released. Reservation happens before
durable writes, and failure or cancellation returns unused capacity. Replacement
requires room for both old and new snapshots until publication. These retention
budgets are not a bound on temporary allocations, execution copies or process
memory. Scheduling, FIFO queues and active-work limits remain caller concerns.

Response-scoped session history records reasoning, messages and calls in inference-round order,
then built-in call outputs. Public output is accumulated separately and is not
appended to retained history twice. Compaction replaces the canonical window while
preserving MCP discovery records needed for orchestration. Durable restoration
and replayed compaction input select the effective compacted window before validating current calls;
obsolete stored rows are not deleted or charged to that retained window.

For response-scoped session execution, `store: false` has no durable writes or
database fallback. A stored child of a transient parent persists the complete
canonical window without creating a row or dangling database reference for that
parent. Explicit `conversation_id` requests keep the existing durable Conversations
policy: only `store: true` appends through the conversation handler. Their session lease
still serializes execution without recording a second copy. Core commit supports
prewarming without inference; WebSocket `generate: false` uses that same session
rehydration and commit path.

### `handler/common.rs`

Transport helpers shared by the HTTP and WS handlers: body reading with a shared size
cap, generic JSON parsing, bearer-token extraction, SSE response wrapping, and
rendering an `ExecutorError` as a JSON error body.

### `auth.rs`

OIDC bearer-token authentication: discovery, JWKS fetch/cache/refresh, and the
`require_oidc` axum middleware layered onto protected routes in `app.rs`. The
WebSocket handler also reads the resulting `AuthenticatedPrincipal` extension directly,
to detect token expiry mid-session. `auth/api_key.rs` holds `require_api_key`, which
guards stored-response retrieval when OIDC is off and `OPENAI_API_KEY` is set, because
that route reads local storage and never reaches an upstream that would check the key.

### Request admission: authentication, then rate limits

`build_router_with_auth` applies `route_layer` to the protected routes twice. Axum runs
the layer added last first, so the rate-limit layer is added before the OIDC layer, and
every protected request is admitted in this order:

1. `auth::require_oidc` (when OIDC is configured) verifies the bearer token and inserts
   the `AuthenticatedPrincipal`, or rejects with 401 (503 if the identity provider is
   unavailable).
2. `rate_limit::enforce_rate_limit` (when a rate is configured) charges the request to
   its caller's bucket, or rejects with 429.
3. On `GET /v1/responses/{response_id}` only, `auth::api_key::require_api_key` (OIDC
   off, `OPENAI_API_KEY` set).
4. The handler.

Authentication must come first because the limiter keys a bucket by the verified
principal whenever there is one. With OIDC, an unauthenticated request is rejected
before it can consume any caller's allowance.

`/health`, `/ready`, and the OpenAPI docs are public routes outside both layers. The
outermost `track_request` layer still records 401 and 429 responses in HTTP metrics.

### `rate_limit.rs`

Opt-in, per-process, in-memory rate limiting with one bucket per caller.

- **Algorithm.** `RateLimiter` implements GCRA, which is equivalent to a token bucket.
  Each bucket is a single theoretical arrival time in a `Mutex`-guarded map
  (`LimiterState::arrivals`). The critical section contains no `.await`.
- **Keys.** `RateLimitKey::for_caller` uses the OIDC principal (`issuer` + `subject`)
  when there is one. Otherwise it uses the connection's peer address from
  `ConnectInfo`, with IPv6 grouped by /64. That address is why `server.rs` serves with
  connect info.
- **What never chooses a bucket.** A bearer token or `x-api-key` the gateway does not
  verify, and forwarded-for headers. A client could vary them on every request.
- **Memory bound.** The key map holds at most `MAX_KEYS` callers. When it is full, idle
  buckets are swept at most once per `SWEEP_INTERVAL`. Callers beyond the cap share the
  `Overflow` bucket, which keeps memory bounded under key churn.
- **Rejections.** A rejection carries `retry-after`, `retry-after-ms`, and
  `x-ratelimit-*-requests` hints, all rounded up.
- **WebSocket.** The middleware charges the upgrade request. It also inserts a
  `RateLimitCharge` extension, which the Responses WebSocket handler uses to charge each
  `response.create` frame to the same bucket. A rejected frame is answered with an
  error event, and the connection stays open.

### `error_response.rs`

The single builder for errors the gateway raises itself rather than an upstream or the
executor: authentication failures (`auth.rs`) and rate limits (`rate_limit.rs`).
`GatewayError` carries the status, the OpenAI and Anthropic `error.type` values, and a
message. `ErrorFormat::for_path` picks the envelope: Messages routes answer in the
Anthropic format, with a generated `request_id` repeated in the `request-id` header;
every other route answers in the OpenAI format. Both envelopes are typed `Serialize`
structs, not ad hoc JSON. Callers add their own headers, such as `WWW-Authenticate` or
the retry hints. Executor errors keep going through `handler/common.rs`.

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
`types/io/input` also owns the one model-facing projection of input: every upstream request
goes through it, so it lowers the public records the model never read (search items, compaction checkpoints).

- **`types/request_response.rs`** — `RequestPayload` is the deserialized incoming
  request. Its `to_upstream_request(&self, stream: bool) -> Result<UpstreamRequest<'_>, UpstreamRequestError>`
  is the seam between the OpenAI-shaped request and vLLM's contract. It validates
  message content through `ResponsesInput::validate()` before normalization, then flattens Codex
  namespace tool members to model-visible names (into the tool layer's `ToolDeclaration`s),
  validates every declared tool (`ToolDeclaration::validate()`), and normalizes each
  supported model-visible tool to `UpstreamTool::Function`
  (`ToolDeclaration::to_function_tools()`). File search and
  unknown typed declarations normalize to no upstream tool; a code interpreter
  declaration normalizes to a fixed function contract. The server request path
  checks runtime availability before it calls this conversion. Every
  declaration that does reach vLLM is `type: "function"`, because that's the only
  tool type it speaks. The conversion also resolves/validates `tool_choice`
  and applies `ResponsesInput::normalized_model_input`, the model projection: each
  `tool_search_call`/`tool_search_output` becomes the function call and output the model saw. It's called from
  `executor/upstream.rs`'s `fetch_blocking_payload` and `fetch_stream_payload` — the
  two functions that actually build the outbound request to vLLM.
  Input validation also runs before and after rehydration. It is separate from
  deserialization because raw proxy routing preserves unsupported content for the
  upstream to interpret; typed construction reports input and tool errors separately.
- **`types/io/`** — `input.rs` (inbound message/tool-call/tool-result shapes,
  `ResponsesInput`), `output.rs` (outbound output items: messages, function calls, web
  search/MCP calls, reasoning — plus the `ApplyDone` trait described below), `tools.rs`
  (the normalized `FunctionTool` and `ToolChoice`, distinct from tool *declarations*),
  `reasoning.rs` (typed reasoning content, summaries, item status, and bounded opaque
  state), and `usage.rs` (token accounting structs). `ResponsesInput::model_input()` (through
  `normalized_model_input`) is the final model-visibility boundary used by `RequestPayload::to_upstream_request`.
  `input/search_history.rs` pairs search items (a client item by its `call_id`, a hosted output with the oldest open
  hosted call, whose item id is the model's call id) and lowers them. A replayed `mcp_call` or `web_search_call` is
  not lowered yet and reaches the model as the public record. It removes orchestration-only `McpListTools`,
  `CompactionTrigger`, and public collaboration input items. A persisted
  `Compaction` item is different: the latest checkpoint supersedes earlier model
  context and is converted into an assistant `output_text` summary, while canonical
  retained user messages and items after the checkpoint remain. This keeps rich
  continuation state available to orchestration without sending unsupported public
  item types to vLLM.
  Message-file conversion belongs to `input/file_text.rs`: supported inline UTF-8
  text files become model-facing text with their `prompt_cache_breakpoint` retained.
  Canonical history keeps the original file; unrelated file extensions are not copied
  onto the text part.

- **`types/io/item.rs`** — `InOutItem` is the shared input/output union used by
  `ResponsesInput::Items`, conversation requests/resources, and retained history.
  Submitted history is decoded through the derived `InputItem` decoder, including
  typed web-search and MCP call records. `InOutItem` derives deserialization through
  `From<InputItem>`; upstream output decoding remains separate and can accept partial
  streaming shapes. Its request schema describes the same strict input contract. `into_canonical_items` preserves these public records, while
  `InOutItem::into_input_items` uses the existing output-to-input conversion and
  client-tool lowering for model history. Storage row markers and legacy decoding
  remain in `storage/`; there is no separate conversation-item enum.
  Item retrieval and listing return `InOutItem` directly. `into_public` assigns the
  storage identity and expands shorthand messages on the returned item;
  serialization uses the item types themselves, without an item-response wrapper.
  `ResponsesInput::input_items` uses `InOutItem::as_input` to borrow submitted input
  items and project outputs. The existing compaction-window selection accepts these
  borrowed items, so estimation avoids copying message and image payloads and model
  construction moves owned projections.
- **`types/io/multi_agent.rs`** — public collaboration output items and agent-message
  content. Its `input` module defines replay forms with optional IDs; output IDs remain
  required. Agent-message content includes text, images, files, refusals and encrypted
  parts, preserving response content during replay. These wire types do not impose a
  storage policy; stored IDs restore metadata, while stateless requests supply the complete history.
- **`types/tools/params.rs`** — the tool **declaration** shapes a client sends:
  `ResponsesTool` (tagged enum: `Function`, `ToolSearch`, `Mcp`, `WebSearch`, `FileSearch`,
  `CodeInterpreter`, `Namespace`, `Custom`, `Unknown`) and each variant's param struct.
  This is a good concrete example of the module boundary: `ResponsesTool` is *defined*
  here as a pure wire shape; the Responses adapter converts it into the tool layer's
  `ToolDeclaration` (`tool/declaration.rs`, `responses_declarations`), and the
  declaration's behavior — `validate()` and `to_function_tools()` — is implemented on
  that one internal type in `tool/normalize.rs`, which delegates to per-type handlers.
  Types own the shape; tool owns what it means.
- **`types/messages/`** — a separate, parallel type layer for the Anthropic Messages
  API (`MessagesRequest`, `ContentBlock`, etc.). `tools.rs` classifies declarations
  as function tools, supported native web search/fetch tools, or opaque provider
  tools. Supported native variants retain their original version from the shared
  capability tables in `tools/versions.rs`; classification and request validation use
  those same tables. Unknown versions retain their discriminator and extension fields
  without becoming supported native versions.
  New supported kinds extend the enum and its explicit deserialization/serialization
  mapping, then add the corresponding tool-layer conversion and validation.
  `tool_seam.rs` is the pure, I/O-free
  adapter for `tool_use`/`tool_result` blocks and the gateway-ownership map; the
  declarations themselves are mapped by `tool::registry_tools` into the tool layer's
  `ToolDeclaration`, so both APIs build a `ToolRegistry` through one path without the
  Messages loop depending on `RequestPayload`/`ResponsePayload`.
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
- `types/io/item.rs::InOutItem::into_input_items` uses the same method when
  rebuilding continuation input from persisted history. Already stored `InputItem`s
  pass through unchanged.

Messages, reasoning, function calls, custom calls, tool-search calls, compaction
items, and MCP list metadata have explicit continuation representations. Public
`web_search_call` and `mcp_call` output items return `None`: gateway execution records
the canonical model-facing function call and its `function_call_output` separately,
so converting the public projection would duplicate the call or lose result details.
This omission applies to gateway-emitted output records. Client-submitted
`InputItem::WebSearchCall` and `InputItem::McpCall` remain input records throughout
request conversion and storage replay.
Gateway tool results are already `InputItem`s and are appended through
`append_gateway_results`; they do not need an output-to-input conversion. Each result
carries one `input`: the turn's history and the model's next-round input alike. A hosted
(`execution: "server"`) search is the one tool whose input is its public pair: the executor
records the `tool_search_call` and `tool_search_output` in round order, so they also return
`None` here, and the storage writers skip an output whose id the turn's history already holds
(`OutputItem::is_already_in_history`). Multi-agent rounds keep the pair in each agent's
history. The upstream projection lowers the pair the same way live, on continuation, and on
replay, so later prompts share the live round's prefix.

Native upstream `code_interpreter_call` output items convert to typed continuation input. A gateway-executed code-interpreter call is marked internally and omitted by the same conversion because its model-facing function call and output are retained separately; storage preserves that origin outside the public wire shape.

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
   typed `EventFrame` in `executor/gateway.rs` and present it with `StreamRelay::emit_local`.

### `executor/` — the loop, and the server's only door into storage

This is the layer `agentic-server` talks to. It owns the request lifecycle: rehydrate,
call inference, run the tool loop, persist. `agentic-server` never reaches past it.

- **`request.rs`** — `RequestContext` (per-turn state: original + enriched request,
  response/conversation IDs) and `ExecutionContext` (long-lived deps: storage
  handlers, HTTP client, gateway tool executors, LLM base URL). `ExecutionContext` is
  what `AppState` holds; it exposes `conv_handler`/`resp_handler` (the `modes/`
  handlers below), never the raw stores. See [original and enriched requests](#original-and-enriched-requests)
  for the two request copies' responsibilities.
- **`rehydrate.rs`** — `rehydrate_conversation()` loads prior history from either the
  conversation store or the response store depending on which ID the request carries,
  and builds the enriched `RequestContext`. Rehydration retains internal
  `InputItem::McpListTools` records so `ToolRegistry` can suppress repeated MCP
  discovery lifecycle output. After stored history and the new request input are
  combined, `pending_calls.rs` validates the complete continuation's function/custom
  call sequence. A request without stored history runs the same check when its input
  replays search items, since the ledger owns how client search calls and outputs pair;
  tool search keeps only the rules it alone sees (item ids, hosted pairs, output status).
  Every call and call output must have a non-empty `call_id`; call IDs
  must be unique across the sequence; and each output must resolve a call of the same
  item kind. An output whose `call_id` matches no call is rejected with the `OpenAI`
  unknown-call error; a second output for a resolved synchronous call or a
  function/custom kind mismatch is an invalid request rather than evidence that the
  call was resolved. Valid unresolved calls remain ordered by their original emission,
  and the first unresolved synchronous client-executed call produces the existing
  missing-output error. An async call (`async: true` on the call item) may stay
  unresolved and accepts repeated outputs; it ends neither the resolved prefix used by
  compaction nor the continuation. A compaction window (`model_items`,
  `canonical_session_history`) shows the model an async call that was still open at its
  checkpoint. Retained histories (`retain_window`, a session whose history compaction
  replaces, and a multi-agent compaction, before its compaction item) also keep every async
  call and the last output that answers it, outside the model view, so a further output still
  finds its call while repeated outputs do not accumulate. A compacted call's public form
  comes from stored history, not from the current tool declarations.
  Compaction replaces the request's own items, so it stores some of them again, as the
  request supplied them. Before the compaction item (the answered call and its output leave the
  window together) go outputs that answer calls made before the request, and the request's own
  answered async calls with the last output for each; after it go the async calls the request
  made that are still open. A call already stored keeps its place, so no item or `call_id`
  repeats. Multi-agent runs route outputs through the pending-call ledger, which knows only
  the calls the gateway emitted, so there a further output for an async call the client
  supplied itself is rejected, with or without compaction. An async call answered before the
  checkpoint returns to the model view when a further output for it arrives after the
  checkpoint.
  Gateway-executed built-in tool calls are resolved and
  recorded within their originating round, so they do not remain pending at this
  boundary.
- **`upstream.rs`** — the narrow adapter between inference transport and the pipeline.
  It builds `UpstreamRequest`s, snapshots registry classification facts into an owned
  `TranslationContext`, passes the shared retained-data budget into ingestion, and
  passes each live JSON or SSE body to `AgentPipeline`. It does not charge SSE wire bytes.
- **`inference.rs`** — `call_inference()`: the raw HTTP/SSE transport to vLLM. No
  parsing beyond splitting `data: ...` lines and stopping at `[DONE]`.
- **`pipeline.rs`, `pipeline/`** — `AgentPipeline`, the request-owned entry point for
  both response body formats. `RoundIngestion` owns the synchronous per-round semantic
  core; its `StreamRelay` (`relay.rs`, `relay/`) owns awaited, ordered client delivery
  across rounds.
- **`engine.rs`** — the top-level orchestrator: `ExecuteRequest`/`execute()`,
  `create_conversation()`, and `EngineOrchestration`, which owns the response byte
  budget, accumulated output/usage, and root `AgentTurn`. It advances the turn and
  applies response completion policy to its typed `RoundResult`/`RoundDecision`.
  The current single-agent path supplies `MAX_GATEWAY_TOOL_ROUNDS = 10` as a per-turn
  limit, not a future tree-wide collaboration limit. Also home to `run_compaction_trigger`,
  `run_blocking`, and `run_stream` (owns and polls the loop alongside its bounded event
  receiver, forwards events as SSE, and persists before yielding the terminal event).
  `engine/streaming.rs` owns cancellation, failure delivery, and terminal validation
  before persistence; `engine/streaming/producer.rs` drives the stream-owned
  orchestration future without spawning a nested producer task.
- **`engine/agent_turn.rs`** — execution for one agent: `AgentTurn` owns the tool
  registry and round counter and exclusively borrows its `AgentPipeline`. The response
  adapter retains the pipeline so error handling can recover its context and delivery
  state. `run_round` performs compaction, inference and built-in tool execution using
  the engine's shared budget, records canonical call/output history, and reports a
  typed outcome. It does not spawn agents or finalize/persist a public response.
- **`multi_agent.rs`** — scoped task ownership for engine-owned tree coordination.
  `RunOwner` bounds root/descendant tasks, supports interruption, and joins teardown.
  `multi_agent/registry.rs` holds parentage, current turn identity, logical phases and
  bounded canonical mailboxes. It reserves bytes through the shared response budget
  before mutation and settles turns with exactly one parent notification. Registry
  operations are synchronous; the coordinator must reconcile/join work before settlement.
  `multi_agent/pending_calls.rs` retains each client call's originating agent, turn and
  kind. It validates output batches before mutation, retains accepted outputs until
  transfer to canonical context, and keeps resolved IDs for duplicate detection.
  An async call's owner record says so: its owner does not wait for it
  (`awaiting_outputs` lists only synchronous calls), and repeated outputs for it are
  queued and transferred in order. Acceptance by this table does not itself resume an
  agent or decide visibility for superseded turns. It complements the existing
  single-history rehydration validator; neither public error mapping nor
  transport-specific batch policy is defined by it.
  `multi_agent/control.rs` provides opt-in live input through `RunControl::channel`
  and `ExecuteRequest::with_run_control`. It requires a streaming multi-agent
  inference request. Nonblocking admission bounds each batch's logical retained bytes
  and at most 32 outstanding commands, including unread decisions. Adapters must keep
  draining response events while awaiting decisions. The coordinator validates the
  whole batch, transfers outputs into canonical histories, invalidates stale compaction
  generations, and resumes a waiting owner only after all of its synchronous calls are
  resolved. A late async output owned by the root resumes an idle root with a fresh turn
  when it arrives live, and any settled root in a continuation request. An idle agent with a
  client output after its last produced item starts a fresh turn (`resume_unread_agents`) when
  the run starts, after live input, and whenever a turn settles or is interrupted. This covers
  an output that arrived while the agent's last round ran on older input: the round completes,
  the agent settles, and it starts again to read the output. A subagent starts only while
  `max_concurrent_subagents` allows (the root is not counted), and its answer then reaches its
  parent as mail. The rule reads stored history, so an output that arrives at the limit waits
  only for a free slot. A busy or waiting owner reads the output at its next turn. Interrupted
  and failed agents, and live interrupted owners, retain input without being restarted.
  Without a control endpoint, HTTP execution still completes at the client-output boundary.
  Finalization closes admission and rejects queued commands synchronously. `Finalizing`
  is distinct from successful persistence; a lost reply is an execution failure with
  unknown acceptance, never permission to retry silently. This core seam does not yet
  own WebSocket routing, wire acknowledgements, or completed-response retention;
  those belong to the connection adapter and retained event sink described above.
  `engine/multi_agent.rs` coordinates these components for HTTP responses with either `store` value.
  Canonical histories and pending calls remain owned by the coordinator; the pipeline never
  spawns subagents. Its `context`, `actions`, `rounds`, `delivery`, and `compaction`
  modules separate restoration, collaboration commands, scheduling, public output and
  explicit root compaction. The run owner cancels and joins tasks when a run finishes or
  its retained owner cancels it; dropping an HTTP stream aborts them without joining.
  Interruption takes effect at the current round boundary. The contracts are described below.
- **`persist.rs`** — `persist_response`/`persist_turn`, which apply the request's
  storage policy (`should_persist`: a no-session `store: false` turn is not written) and
  route to `ConversationHandler` or `ResponseHandler` in `modes/` depending on whether the
  turn is conversation-scoped or response-scoped.
- **`compaction.rs`** — `compact_response()` (the explicit `/v1/responses/compact`
  path) and `maybe_compact_context()` (automatic, threshold-triggered, called from the
  round loop before each inference call).
- **`modes/conversation.rs`, `modes/response.rs`** — `ConversationHandler` and
  `ResponseHandler`. Thin, 1:1 wrappers around `storage::ConversationStore` /
  `storage::ResponseStore` that translate `RequestContext` into store calls and
  `StorageError` into `ExecutorError`. **This is the sanctioned boundary between the
  executor and the storage stores** — nothing above this layer touches
  `storage::conversation`/`storage::response` directly. Today they only cover what the
  pipeline and retrieval need (`get`, `get_or_create`, `create`, `rehydrate[_snapshot]`,
  `execute_turn`, `commit_tree`, `retrieve`, `validate_exists`); **any new CRUD operation
  beyond persist/rehydrate belongs here**, added as a new method that delegates to the corresponding store.
- **`error.rs`** — `ExecutorError`, with the mapping methods (`http_status()`,
  `error_type()`, `into_response_body()`, ...) handlers use to render errors.

#### Original and enriched requests

`RequestContext` holds two `RequestPayload` values. Rehydration starts them from
the same client request. Choose the copy according to the operation:

- Use **`original_request`** for the current client's input and request policy:
  what the client supplied, whether to store the response, which response or
  conversation to continue, and which request settings to report publicly.
- Use **`enriched_request`** for the effective execution state: resolved history,
  inherited settings, prepared tool declarations, and the settings for the next
  model invocation. This copy changes as the tool loop progresses.

Neither rule means that all public metadata comes from the original copy or that
all persisted data comes from the enriched copy. Use the field-specific rules below.

##### Which source to use

| Operation | Read or modify | Reason and implementation |
|---|---|---|
| Validate the client's submitted fields and new input | The incoming `RequestPayload`, then `original_request` during rehydration | Check request policy before loading state. For example, reject simultaneous `conversation` and `previous_response_id`. |
| Decide whether a setting was omitted and may be inherited | `original_request.tools` and `original_request.tool_choice` during rehydration | An explicit tool list selects the client's own selector or its default. An omitted list may inherit stored tools. See `resolve_tools` and `resolve_tool_choice`. |
| Validate what will actually execute | `enriched_request` after rehydration and tool preparation | Inherited tools and restored history must also be valid; validating only the submitted fields misses them. |
| Append the current client's input to restored history | Read `original_request.input`; write the combined history to `enriched_request.input` | The original input contains this request's submission; the enriched input contains the model context. |
| Append tool calls/results, prepare reasoning, estimate context size, or compact model input | `enriched_request.input` | These operations affect the next inference round. Use the existing history/compaction helpers so canonical persistence state is updated consistently too. |
| Build the model request | `enriched_request.to_upstream_request(stream)` | The model needs resolved history, tools, and the current round's selector. Converting the original copy would omit inherited history and tool results. |
| Decide storage policy and route persistence | `original_request.store` and `original_request.conversation`, together with `ctx.continuation` | `should_persist` allows connection-local checkpoints for response-scoped sessions; explicit conversations honor `store: false`. Use that helper rather than checking either request's `store` field alone. |
| Identify the parent response publicly or in stored metadata | `original_request.previous_response_id` | Rehydration clears the enriched copy's parent ID after inlining its history. The public response still identifies its parent. Generated response/conversation IDs come from `RequestContext`. |
| Populate ordinary request-derived response fields | `original_request` through `StandardResponseFields::apply_request` and `RequestContext::inject_ids` | This covers fields such as `store`, `metadata`, `text`, and `max_output_tokens`; use the existing field-specific defaulting. Upstream-derived fields such as usage and the delivered `service_tier` come from execution results. |
| Populate public response `tools` | Active `ToolSearchState::public_response_tools()`, otherwise `enriched_request.tools`, through response metadata sanitization | Use effective public declarations, including inherited tools. Remove private MCP discovery/auth fields. The original list can be absent; the upstream list contains lowered function schemas. |
| Populate public response `tool_choice` | `RequestContext::public_tool_choice()`: `original_request.tool_choice`, falling back to `enriched_request.tool_choice` when absent | Preserve the submitted or inherited selector across the public lifecycle. Retain an inherited value on the original copy before relaxing the enriched selector; see below. |
| Store items/history | `new_input_items`, finalized output items, and the existing session/conversation persistence helpers | Neither request's entire `input` is a generic list of new rows to insert. Persisting enriched history wholesale would repeat prior items. |
| Store the selector for later continuation | Move `original_request.tool_choice`, falling back to `enriched_request.tool_choice`, in every response/conversation metadata writer | Use the same precedence as `RequestContext::public_tool_choice()`, taking ownership because execution is complete. Never save the temporary `auto` introduced by internal tool execution as the next request's default. |
| Store other effective settings for later continuation | `enriched_request.model`, `.tools`, and `.instructions` in `persist_prepared_turn` | Tool-search metadata supplies its own effective tools and loaded definitions. Persisting the original request wholesale would lose inherited settings and prepared tools. |
| Retrieve an already stored public response | The stored `response_snapshot` when present | Preserve the public payload that was returned. Do not rebuild its selector from a later-round inference setting. |

##### Mutation and conversion boundaries

`original_request` retains the client input and request policy, but it is not an
immutable wire capture. The deliberate selector exception is
`RequestContext::relax_tool_choice()`: it replaces the enriched selector with
`"auto"` and moves the previous resolved selector into the original request if that
field was absent. This transition does not clone the selector. After this step,
`original_request.tool_choice.is_some()` no longer proves that the client explicitly
supplied the field. Make inheritance and omission decisions during rehydration,
before this mutation. Persistence may also
move fields out of the context once execution is complete.

Apply per-round inference changes to `enriched_request` before calling
`to_upstream_request`. The conversion reads `self.tool_choice` and the other fields
on that payload; it does not accept a second selector argument. It creates a
separate `UpstreamRequest`, lowering tool declarations and selectors into the forms
the model understands and omitting gateway-managed state fields. It does not replace
`enriched_request.tools` with those lowered schemas. `upstream.rs` can then filter
withheld tools or append agent guidance to the temporary upstream request without
changing the retained public declarations or canonical history.

`AgentPipeline` owns ingestion and delivery across inference rounds. Request settings
remain on `RequestContext`; the pipeline has no separate `tool_choice` field.

##### Example: required tool choice across two inference rounds

For a client request with `tool_choice: "required"`, a gateway-executed tool can
run in the first round and the model can answer in the second:

| Stage | `original_request.tool_choice` | `enriched_request.tool_choice` | Public response selector |
|---|---|---|---|
| First inference | `required` | `required` | `required` on creation events |
| After `relax_tool_choice()` | `required` | `auto` | `required` |
| Follow-up inference and completion | `required` | `auto` | `required` on the terminal event and final JSON payload |

If the client omitted the selector and inherited `required`, the original field
starts as `None`. Translation initially falls back to the enriched `required`.
Before the second round, `relax_tool_choice()` retains `required` on the original
copy, giving the same final two rows above. The JSON and streaming paths use the
same translation rule.

The stored response snapshot and `ResponseMetadata.effective_tool_choice` both retain
`required`. A new client request starts with fresh original and enriched copies. If
it continues this response and omits both `tools` and `tool_choice`, rehydration
restores `required`; its first model invocation must not inherit the previous
request's temporary `auto`. An explicit selector on the new request overrides the
stored value. An explicit replacement tool list with no selector uses the default
`auto`, so an inherited selector cannot refer to removed tools. These rules apply
to durable response continuation and transient session checkpoints; no previously
mutated request context is reused.

##### Example: inherited tools in public response metadata

A continuation can omit `tools`, leaving `original_request.tools == None`, while
rehydration restores a `web_search_preview` declaration into `enriched_request.tools`.
The next upstream request lowers that declaration to a function schema. Public
response metadata must still show `web_search_preview`: reading the original copy
would lose the inherited tool, and echoing `UpstreamRequest.tools` would expose the
internal function schema. This is why public `tools` uses the enriched declarations
(or the active tool-search projection), while public `tool_choice` prefers the
retained original selector. Always apply the existing metadata sanitization before
exposing those declarations.

#### HTTP multi-agent execution

`multi_agent.enabled` selects gateway orchestration; it is not forwarded to the
upstream model. Enabled requests accept either `store` value; the current validator
rejects `max_tool_calls`, `reasoning.summary`, and `reasoning.generate_summary`. These are gateway
validation rules, not a claim that every upstream provider has the same restrictions.
New requests without multi-agent configuration use single-agent execution;
continuations of a stored agent tree inherit its configuration.

```text
EngineOrchestration — one public response and persistence boundary
└─ MultiAgentRun — tree, admission, mailboxes, canonical histories, completion policy
   ├─ RunOwner — scoped round/compaction tasks, cancellation and joining
   └─ Per-agent execution
      ├─ AgentPipeline — context, tool-search state and round ingestion
      └─ AgentTurn / AgentExecutionState — registry, round progress and run_round
```

Parallel tool execution runs multiple calls within one agent's inference round.
Multi-agent execution gives each agent its own history, assignment, tool registry,
loaded tool definitions and round progress. The model requests collaboration through
canonical function tools; `actions.rs` handles spawning, messages, follow-up tasks,
waiting, interruption and listing. `max_concurrent_subagents` limits active descendant
turns across the whole tree, excluding the root; it does not fix the total number of
agents or collaboration calls in a response. Finished turns release capacity.
`guidance.rs` adds current identity, assignment, direct-child states and available
slots to each model round without storing that guidance in canonical history.

The coordinator alone mutates canonical agent state. Workers receive snapshots and
return typed outcomes. `AgentRegistry` records logical phases and mailbox contents;
waiting agents resume on mail or a wait deadline, without holding an inference task
open. Child completion notifies its parent once. A root final answer settles only the
root's turn: descendants continue until they finish or pause for client tool outputs.
Mail to an idle root is retained without restarting it. Root completion does not
implicitly interrupt descendants; interruption remains an explicit collaboration action.
The run shares a retained-data budget and accumulates inference/compaction usage, while separate retention, round
and runtime limits bound work. Teardown cancels and joins scoped tasks before the
terminal response decision is exposed.

**Client tool continuations.** Function, local shell, custom and tool-search calls
can suspend an agent for client execution. `PendingClientCalls` records each call ID,
item kind and owning agent turn outside the replaceable history prefix. A continuation
must supply outputs for every outstanding call; the batch is validated before any
agent history changes. Each accepted output goes only to its owner and resumes that
agent. A parent parked waiting for children stays asleep until mailbox delivery wakes
it; a new user message resumes the root.

Tool-search outputs restore discovered definitions into the requesting agent's tool
catalog, prepared in its `ToolSearchState` and then owned by its registry, before its next
registry and upstream request are built. Namespace
and custom declarations still use the existing canonical function normalization and
public-shape translators. MCP, web search and operator-enabled code interpreter execute in the gateway and append their
canonical call/output pairs during the round, so they require no client output
continuation. Client tool search retains its normal `parallel_tool_calls` validation; enabling
multi-agent execution does not bypass that rule.

Hosted tool search runs in the gateway like those tools. Its `tool_search` function is
the one `ToolType::ToolSearch`, gateway-owned because the declaration's `execution` is
`"server"`. Every tool-search definition enters one request-scoped inventory,
`ToolCatalog` (`tool/catalog.rs`): the declarations, then each completed
`tool_search_output` in history order (with `SearchHistory` and the pending-call ledger
checking the pairing), restored loads after compaction, and hosted results. Preparation
builds it, so an invalid history is a 400 before inference. The catalog owns the
model-visible name rule (`ToolCatalog::validate_declared_names`): preparation applies it to
the whole catalog, deferred definitions included, and every registry build applies it to
the request's declarations before it registers or discovers anything. The response's
`ToolRegistry` then owns the catalog, offers the model its loaded definitions, withholds the
deferred ones (the round's call check refuses a call to one before any tool runs, behind
ingestion's rejection), and runs a hosted search over it: deferred
functions, namespace members, and deferred MCP servers (as `mcp_<server_label>`, with the
members MCP discovery listed), excluding tools loaded by earlier responses. The search is
pure, so its `tool_search_output` is known when the call is planned: the scheduler gives
the call two public output indexes (a follow-up item), and later items in the round shift
by one, the inverse of an omitted refusal. After the round,
`ToolRegistry::load_searched_tools` loads the returned tools into the catalog, renders the
next round's offered tools, and adds client entries for loaded namespace members without
listing MCP servers again; `ToolSearchState` keeps only the public snapshot persistence
needs. A deferred MCP server stays declared with `loaded_tool_names`, which limits the
tools normalization sends upstream.

Stateless continuations rebuild each agent's catalog from the complete client-returned public history. Hosted
search outputs retain their `agent` attribution, so a child's call and output load tools only for that child;
only client-executed searches enter the pending-client-call ledger. MCP discovery records remain response-scoped,
including for children created with `fork_turns: "none"`, and are reused independently of a child's loaded subset.
`RequestPayload::to_upstream_request` builds its naming context directly from the request. Compaction uses that
existing namespace resolver with the source's known top-level functions before summarization. The summary offers
no tools and retains actual namespace members. Repeated compaction therefore preserves names without a separate
pipeline naming context or conversion API.

Code interpreter uses the same canonical function/tool loop in every agent. Agents
share the ready executor and its process-wide guest permits through `ExecutionContext`;
`max_concurrent_subagents` does not increase interpreter capacity. Its public
`code_interpreter_call` items and streaming events are attributed to the executing
agent and use the response-wide output indexes. Canonical call/output pairs survive
stored tree continuation, so submitting a sibling's client tool output does not
execute completed Python calls again. Feature, operator and readiness gates apply
before inference, including when the tool declaration is inherited from storage.

**Compaction.** Automatic compaction is per agent. `prepare_agent` supplies a default
compaction threshold of 100,000 estimated tokens when none is configured. A
`CompactionPlan` owns a resolved-prefix snapshot, agent identity and context generation.
Its completion replaces that prefix only when the generation still matches
(`CompactionCommit::Applied`); a stale result leaves current history intact. The
coordinator accounts for completed compaction work once even when the result is stale.
Pending calls, unresolved suffixes and new mail are not discarded by prefix replacement.

An explicit `compaction_trigger` in a Responses request uses
`MultiAgentRun::compact_root`: it restores the tree, applies supplied continuation
input and compacts the root's resolved prefix while preserving child histories and
pending calls. No agent tasks run during this operation. The resulting tree passes
through the same validation and persistence boundary as a normal multi-agent response.
This is distinct from the standalone `/v1/responses/compact` endpoint.

**Persistence and public output.** A versioned `StoredTreeSnapshot` contains agent
histories, phases, mailboxes, loaded tools and pending client calls.
`ValidatedTreeCheckpoint` checks its limits, identities and references before live
state is constructed or committed. `persist.rs` calls the existing mode handler's
`commit_tree`; transport handlers do not write storage directly, and tree persistence
uses the existing response/conversation stores.
`ResponseMetadata.multi_agent_tree` restores execution state, while
`response_snapshot` retains the exact terminal public response for GET retrieval.
Continuation restoration discards the latter duplicate but retains the tree.

With `store:false`, HTTP execution writes no response, item, conversation, or tree rows.
Without stored IDs, `rehydrate::from_responses` normalizes the complete client-supplied
input without loading stored state or decoding a tree snapshot. The coordinator's
`history.rs` processes standard collaboration items, agent attribution and client tool
outputs using the existing registry, mailbox and pending-call types. It constructs
request-local agent contexts and resolves each client output by call ID and kind.
Historical interruptions settle after the interrupted round's public output has
been retained, before applying the next client input. That output remains in the
agent's history without restarting the interrupted turn; an explicit follow-up
can start a new turn.
When enabling multi-agent on a stored single-agent response or conversation,
attributed input is processed together with the complete resolved history, so
earlier user and assistant items remain available.
There is no private replay envelope or mandatory compaction at the response boundary.
Actual automatic per-agent compaction remains separate. Explicit stored parent IDs and
conversations can still restore an internal tree; `store:false` leaves the new turn unstored.

`collaboration::TranscriptSealer` protects inter-agent message text, using the same key
across requests so clients can return the gateway's encrypted messages in their input.
`AGENTIC_MULTI_AGENT_TRANSCRIPT_KEY` accepts a base64-encoded 32-byte deployment secret;
use the same key across replicas and restarts. The default key lasts for the current process.
Only the key survives stateless requests, not their histories, trees or pending calls.

The public transcript contains attributed `multi_agent_call` (`mac_`),
`multi_agent_call_output` (`maco_`) and `agent_message` (`amsg_`) items alongside each
agent's reasoning, messages and tool calls. Public collaboration items are not replayed
as model input; the coordinator retains canonical function calls, outputs and delivered
mail in private agent histories. `ResponsesInput::model_input` removes the public
collaboration items at the upstream boundary.

Both JSON and SSE use this coordinator. Streaming workers feed a bounded,
acknowledged channel into the existing response-wide `StreamRelay`.
`AgentRoundId` scopes source indexes; delivery's public-position mapping is shared
with final output assembly. The coordinator emits one response lifecycle and completes
items after canonical registration and public projection. Delivery does not become a
second response assembler. An item that agent frames already presented needs only
`output_item.done`. An item the coordinator creates itself, which no upstream frame
presented, gets its whole public lifecycle from `item_lifecycle.rs`. That module builds
each frame lazily from the completed item and decides per item kind, through an
exhaustive match, what streams between `output_item.added` and `output_item.done`.
The relay attributes and presents those frames.

**Recorded contract checks.** `tests/multi_agent_contract_test.rs` loads independently
recorded OpenAI and gateway YAML for review, proposals, mixed tools and client-executed
tools in both streaming and non-streaming modes. The typed comparison helper checks
call/agent relationships, lifecycle consistency and required tool kinds; the client
scenario also checks tool-search, discovered functions and custom tool continuations.
It permits model-dependent names, text, identifiers and action counts rather than
requiring byte-for-byte output equality. Unit tests use synthetic values; cassette
capture remains in the Python recorder and scenario shell script.

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
                   StreamRelay
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
| Request pipeline (`pipeline.rs`) | Hold one request context, tool-search state, the response's stream relay, and the JSON/SSE body entry points. |
| Round ingestion (`pipeline/ingest.rs`) | Process one body with one `ResponseAccumulator`, one `TranslationDispatcher`, and final response normalization. |
| Stream relay (`relay.rs`) | Own the client sink, presentation state, round offset, deferred buffer, and their limits; present local and upstream frames through one send path. |
| Agent execution (`engine/agent_turn.rs`) | Own the per-agent registry and round progress; perform inference/tool work and return typed round outcomes. |
| Orchestration (`engine.rs`) | Own agent execution, the shared response budget, response assembly, terminal policy, and persistence coordination. |

`AgentPipeline` currently lives for the complete public response. `EngineOrchestration`
creates the shared response budget and an `AgentTurn` that retains its own registry
and exclusively borrows the pipeline. Each turn step asks `upstream.rs` to run the
inference body through `run_with_json_body` or live `run_with_stream_body`. A new
`RoundIngestion` is created for every body and consumed by finalization, while
`StreamRelay` and its `GatewayStreamAccumulator` survive across inference rounds.

For multi-agent execution, the engine owns tree coordination and schedules separate
agent execution contexts. Per-agent ingestion feeds typed frames through a bounded,
acknowledged channel into one response-wide `StreamRelay`. Its source-scoped index
mapping is shared with the engine's final response assembly; delivery does not assemble items.

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
- Translation consumes validated `EventFrame`s, the accumulator's resolved item kind,
  and its typed function call view, and produces the public wire lifecycle for client-owned tools.
- Delivery consumes translated frames and provides ordered, awaited client emission
  across inference rounds.

Telemetry follows the same owners (`executor/telemetry/`; see
[Observability](docs/deploying/observability.md)). The `ExecutionSpan` guard finalizes
the `agentic.execute` span and the execution metrics together, exactly once, from `Drop`.
Instruments come from `ExecutorMetrics` on `ExecutionContext`. The first-data timings
share an `ExecutionClock`. The pipeline entry marks the first upstream line; multi-agent
rounds run on their own pipelines, which share the execution's clock. The client-facing
stream marks the first event and first output-text delta it yields to the transport, and
its `InstrumentedStream` wrapper measures delivery wait. For retained (WebSocket)
responses that stream is the event stream `ResponseEventSink::stream` returns. Stage timers wrap
the code that owns each stage. A refactor that moves one of these boundaries must move its
mark with it rather than add a second one.

#### `accumulator/` — typed response assembly

`ResponseAccumulator` owns validation, response lifecycle, typed output slots, delta
folding, terminal error/incomplete state, usage, and final `ResponsePayload` assembly.
`slot.rs` owns active/completed lifecycle and identity bindings; its private
`slot/identity.rs` resolves slots without mutation, and `identity.rs` extracts semantic
identities. `json.rs` owns strict JSON response-shape validation, decoding, and accounting.
`active.rs` dispatches exhaustively to per-kind state; `active/shell.rs` owns shell-command
streaming, and `active_text.rs` owns message parts and reasoning text/summary accounting.
`terminal.rs` owns terminal reconciliation, finalization, and steering cuts. Both JSON
and SSE use the same finalization state.

Shared `response.content_part.*` events identify an item by output index and item ID,
not by content type. Normalization parses a typed completed part; the existing slot
resolves whether it belongs to a message or an agent message. Each owner's state
validates the allowed content, duplicate completion and consistency with the final item.

Output items are constructed through their `TryFrom<&EventPayload>` implementations in
`types/io/output.rs`. Active slots fold deltas in place and use the type's `ApplyDone`
implementation when its completion event arrives. For an already parsed output-item
completion, the private `MergeDone` implementations in `accumulator/completion.rs`
merge the concrete item with its retained fields and buffers. `slot.rs` dispatches
to those implementations after validating identity and lifecycle. Finalization
promotes each completed typed item once and preserves validated output-index order.

`ActiveItem::validate_completion` also reports whether the observed completion
proves that in-flight work has settled. It uses the item's existing typed status
and content/identity validation; raw field-presence checks distinguish actual
fields from serde defaults. `SlotMap::complete` combines that evidence with
agreement between the submitted completion and the merged item. Its
`completion_uncertain` flag records inconsistent call IDs, unknown openings or
recovered snapshots. A steering boundary requires at least one observed slot,
no active slots and no uncertainty. Lenient done-only recovery without an opening
continues to ordinary terminal processing rather than authorizing an early cut.
Steering therefore extends the existing completion transitions and consuming
finalizer; it has no second parser, completion classifier or output-item hierarchy.
Only received usage is retained; a cut round may lack terminal usage accounting.

The pipeline's streaming entry is `process_line(ClassifiedSseLine)`. It normalizes and
validates a data line, applies the event to the slot keyed by its validated output
index, and returns at most one validated `EventFrame` for translation. For function
events, `accumulated_function_call(output_index)` exposes a borrowed typed call and its
folded arguments. `finish` applies SSE end-of-stream policy; `finalize` preserves the
status loaded from a complete JSON body.

When adding an output-item kind, extend its typed construction and completion logic in
`types/io/output.rs`, the variants and exhaustive dispatch in `accumulator/active.rs`,
and completed-item measurement in `executor/response_budget.rs`.

Retained-byte accounting stays in synchronous ingestion. `response_budget.rs` defines
one comprehensive `RetainedSize` measurement and `RetainedAccount` for charging growth
and reconciling completed items. Its private `response_budget/input.rs` implements
canonical input and accepted-steering measurement through that same trait. Every unbounded collection entry has a structural
charge, including empty JSON values and web-search queries; unrestricted string fields
such as message `role` and message content `type` count by length. Bounded enums,
including reasoning content kinds and item status, need no variable charge. Reasoning
text and summary parts each charge a container plus text bytes; opaque state charges
its decoded string bytes. Delta text and new part containers are charged before growth.
Completion uses the existing `ApplyDone`/`MergeDone` policy and measures its effect;
reasoning text/summary completion measures only the inserted part and its corresponding
streamed counter; shell command completion measures only its command and current buffer.
Both avoid rescanning preceding parts or static nested metadata. Full measurement is reserved
for item completion and final reconciliation. `slot.rs` charges supplied identities before
index insertion or late binding; IDs shared by a typed item and its indexes count once
logically, including pending kinds with no typed snapshot yet. `accumulator/details.rs`
charges retained terminal errors and incomplete reasons under the same JSON/SSE measurement.
Charges are cumulative without refunds.
A rejected completion may temporarily allocate its single bounded wire payload before
ingestion drops the failed round.

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
the function-call shape; custom, namespace, and tool-search translators restore their
specific public forms.

The private `HasTranslator` association lives in the executor so tool handlers do not
depend on SSE or executor types. `TranslationContext` is an owned snapshot of the
registry facts and request metadata needed for classification and restoration. It
provides each round's tool classification and owns final restoration of public tool
declarations, custom/namespace shapes, tool choice, and tool-search metadata.
Nonempty provider message IDs without the public `msg` prefix receive `msg_` during
translation. Message events and final JSON/SSE snapshots use the same public ID;
ingestion retains the provider's original identity for lifecycle validation. Client
message-ID validation remains strict, so returned history can be submitted unchanged.
Output-text `logprobs` use the existing nullable-vector deserializer: missing and
null values become an empty vector and serialize as `[]`. Wire normalization uses
the same default so content parts agree with item and terminal snapshots;
populated log probabilities are preserved through continuation and storage.

Async client tools are marked from the same snapshot: the registry's async tool names
(`ToolRegistry::async_tool_names`) are copied into the context. The dispatcher creates each
call's translator with its `CallExecution`; for an async call, the function and custom
translators build the public `output_item.added`/`done` items from the typed accumulated call
and mark them with `mark_async_call`, which final output normalization also uses on the
round's call items, before namespace restoration renames them.
Later stages read the marker from the call item itself.

Gateway-executed types (`Mcp`, `WebSearch`, `FileSearch`, and `CodeInterpreter`) are
classified as gateway calls by the dispatcher, which suppresses their canonical
upstream function-call lifecycle. `gateway.rs` owns their execution result mapping and
synthesizes their public item lifecycle because the gateway knows when execution
starts, completes, or fails. `GatewayStreamAccumulator` then assigns cross-round
sequence numbers, rebases output indexes, and deduplicates response start events.
Events that arrive before a function name is known are buffered with a 256 KiB total
byte limit and replayed when the call resolves.

Shell functions are restored to `shell_call` and command SSE events for client
execution. Shell is not gateway-executed: `ShellHandler` deliberately does not
implement `GatewayExecutor`, so the gateway never runs shell commands on the
model's behalf. PR #264 removed the optional shell executor to keep execution on
the client; RFC #352 proposes a gateway-executed shell backend as future work.
The client executes each call and submits a `shell_call_output` item with the
same `call_id`.

#### `relay.rs` and `gateway_accumulator.rs` — continuous client SSE

`StreamRelay`, owned by `AgentPipeline` for the whole response, is the only owner of
the client sink, cross-round presentation state, the current round's output offset,
the deferred-frame buffer, and `RelayLimits`. Callers state a frame's origin by the
method they call; none of them passes a sender, accumulator, or offset:

| Method | Frames | Transforms |
| --- | --- | --- |
| `accept` | One translation from ingestion | Withhold terminal response events for the engine, defer or present each frame, then release what a moved defer window no longer hides. |
| `emit_upstream` | One translated provider frame | Restore the public response IDs and rebase the output index onto the round. |
| `emit_local` | Gateway-synthesized and multi-agent frames | None: they already carry public IDs and absolute indexes. |
| `release_deferred` | Deferred provider frames that a `Release` admits | As `emit_upstream`. |

`begin_round` sets the round's offset and rejects a new round while the previous round
still holds deferred frames. Every method reaches the client through one private
`send`, the only place an output offset is applied. The relay's constructor fixes its
sink: `StreamRelay::client` for the bounded SSE channel, `response` for a retained
WebSocket response's `ResponseEventSink`, `agent` for a multi-agent round's
`AgentFrameSink`, and `detached` for collect-only and JSON execution. `AgentPipeline::new`
takes the configured relay, so the pipeline never handles a sender.
Sequence and response-start deduplication state is committed after the sink accepts
the event, so a failed serialization, closed receiver, or cancelled send cannot consume
it. Enqueueing is not client receipt or playback acknowledgement, and this does not
make an interrupted pipeline resumable or support retrying an already rebased frame.

The relay defers provider frames at and after the first hidden gateway-call index, and
index-less frames while that window is open. The buffer keeps release order on entry:
indexed frames by output index, then index-less frames, each in arrival order.
`RelayLimits` bounds it by 1024 entries and 256 KiB of serialized wire data by default.
A single frame over the byte limit is rejected on its own terms, and no limit drops a
frame. A frame's charged size is refunded only when the sink accepts it, so a cancelled
or failed release keeps the remaining frames and their byte account. These limits are
separate from response assembly, retained session state, and total process memory. A
slow client applies backpressure through the bounded sink and never moves frames into
the deferred buffer.

`release_deferred` is the one release path. It runs when the defer window moves during
a stream; after gateway execution, once per output item, so each item's deferred frames
follow its synthesized gateway lifecycle; and, when an upstream round fails, before the
terminal event without executing gateway tools.

The Responses `BoxStream` owns its orchestration future directly. It polls that future
and the existing bounded event receiver on the consumer's task; there is no detached
producer, asynchronous abort reaper, or producer `JoinHandle`. When the consumer stops
polling, the orchestration future stops with it. Dropping an unpolled or active stream
synchronously drops its producer, active per-request tool futures, and continuation lease.
A multi-agent run is the exception inside that future: its rounds are tasks owned by
`RunOwner`. They progress only until the bounded frame channel is full, and dropping the
stream drops `RunOwner`, which aborts them without joining; each stops at its next await
point and its result is discarded. Remote services and shared HTTP/MCP connection drivers
still have their own lifetimes; dropping a local future does not undo external side effects.

Producer completion and panic both dispose of the producer future before draining
accepted events in order and exposing one outcome. Panic isolation uses `catch_unwind`
and a typed, static client error; it does not alter the process panic hook. On success,
the engine still validates terminal size and persists/publishes the checkpoint before
yielding completion. Cancellation during storage waits drops the pending persistence
future through the same owner. WebSocket responses run through the retained owner in
`engine/retained.rs`, which owns prepared response startup and the running task,
then cancels and joins execution explicitly; a multi-agent run cancels and joins its
rounds through the coordinator's cancellation arm.
Their `wait_until_idle` lease fences remain in place.

`executor/response_events.rs` retains the response's sequence owner and bounded
delivery queue after execution. Typed steering controls can follow terminal events
without restarting output-item assembly. Its monotonic boundary watch requests a
safe cut without cancelling started work. Creation receipts, execution outcome and
actual task joining are separate facts: only sink publication constructs a receipt,
and only `ResponseRunOwner::join()` marks the producer joined. The adapter uses
both facts to resolve a successor's accepted input exactly once.

`GatewayStreamAccumulator` carries only cross-round presentation state: monotonic
`sequence_number`s, public `output_index` rebasing, and deduplication of response start
events. It holds no `OutputItem` assembly state and no sink. Sending is awaited before
ingestion continues, so sender closure or delivery failure propagates through the live
pipeline. After the producer finishes, `engine/streaming.rs` takes the state back from
the relay and renders the terminal event on a clone, which it discards if persistence
fails; cloning copies a few scalars.

#### `gateway.rs` — the tool-loop's building blocks

The round-by-round loop belongs to `engine.rs::EngineOrchestration`. `gateway.rs`
supplies the planning, execution, projection, and continuation helpers it calls each
round:
- `GatewayScheduler::plan` creates one slot per gateway-owned function call. Each slot
  owns the original item index, public output index, typed `GatewayBinding`, and
  lifecycle projection; a missing executor is represented by an explicit slot rather
  than omitted from a parallel vector. `GatewayScheduler::results_in_order` runs the planned
  calls as a `FuturesOrdered` and yields one `GatewayCallResult` per slot in model call order,
  each as soon as it and every earlier call have finished. A `tokio::sync::Semaphore` from
  `GatewayScheduler::execution_slots` limits active tool executions in that round using
  `tools.max_concurrent_gateway_calls`
  (default `5`, configurable through `AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS`). This
  nonzero setting is carried by the owning `ExecutionContext` into each scheduler;
  the permits are local to the round, not a process-wide or cross-request limit.
  Completion may occur out of order, but results are delivered in model call order.
  `gateway/lifecycle.rs::drive_in_order` runs that stream in a producer joined with the
  event emitter: each result goes into a channel with one slot per call, so a slow client
  never stalls tool execution, and every started call is drained before the round returns,
  even after an error. The permit limit bounds execution, not the number of planned call
  futures waiting for permits.
- A normalized `web_search` function call may batch at most five queries. The JSON
  Schema advertises the ceiling and the handler enforces it again because normalized
  web search currently uses non-strict arguments. Provider searches acquire a shared
  handler semaphore initialized from `tools.max_concurrent_gateway_calls`, preventing
  batched calls from multiplying the configured outbound concurrency. Results remain
  collected in query order for the public `web_search_call.action.queries` projection.
- Each bound call first acquires its optional same-tool exclusion permit, then a
  round execution permit, then a materialization permit shared by cloned scheduler
  policies (also used by MCP discovery, with a limit of 16). Waiting for same-tool
  exclusion does not consume a round permit; waiting for materialization does. A call
  whose plan carries a `PlannedResult` (a hosted search, answered when it is planned) takes
  no permit and is never executed: its result is recorded within the same
  `agentic.tool.execute` span, bounded and budgeted like an executed output. The
  independent 60-second timeout wraps `GatewayBinding::execute` only after all
  permits are acquired. These permit waits do not count toward it, so it is not a
  deadline for the entire round or total call latency. Timeout, execution, and tool-config
  failures become failed tool outputs that can be fed back to the model instead of
  failing the whole response. A tool registered as gateway-owned without an
  implementation (currently file search) likewise produces an error tool result.
- Parallel safety is a per-handler contract. `GatewayExecutor::supports_parallel_execution`
  defaults to `false`; registration turns that into a `GatewayBinding::self_exclusion`
  semaphore. The semaphore serializes only simultaneous calls to the **same
  model-visible tool name**. It never blocks different tools from running concurrently.
  MCP and web search opt into same-tool parallel execution.
- `max_tool_calls` admission lives in `gateway/admission.rs`. `AgentTurn` owns one
  `BuiltInToolCallBudget` per response, and `GatewayScheduler::plan_with_budget` admits
  gateway-executed calls in output order before any call runs, so concurrency cannot exceed
  the limit. A call over the limit is a refused slot: it never reaches its handler, opens no
  `agentic.tool.execute` span, and returns the limit message as its tool call output. The first
  refused call with a public item keeps its started item (`web_search_call` at `searching`,
  `code_interpreter_call` at `interpreting`); refused MCP calls and later refusals have no public item, and
  `public_item_index` keeps later public indexes contiguous for deferred frames. After a
  refusal, `AgentPipeline` withholds gateway-executed tools from later upstream requests
  without changing the persisted `enriched_request.tools`.
- Each scheduler slot retains its `GatewayEventPlan`; `gateway/lifecycle.rs`'s `emit_gateway_start_events` and
  `emit_gateway_completed_events` synthesize public lifecycle events for gateway-executed
  web search, MCP, and optional code-interpreter calls from those same slots. A code-interpreter
  call emits `output_item.added`, `code_interpreter_call.in_progress`, the
  `code_interpreter_call_code.delta`/`done` pair, `code_interpreter_call.interpreting`,
  `code_interpreter_call.completed`, and `output_item.done`, with indexes from the plan and
  sequence numbers from `StreamRelay`. The dispatcher suppresses the canonical upstream
  `function_call` lifecycle after classifying the call as gateway-executed. Native upstream
  `code_interpreter_call` items instead follow the accumulator's typed lifecycle and pass through;
  contradictory item kinds at one output index are handled by ingestion before translation. The
  ordinary path (`emit_calls_in_order`) announces the calls up to the first follow-up item (a
  hosted `tool_search_output`, from the call's `PlannedResult`), executes the round
  concurrently, and emits each completed/failed
  lifecycle in call order as soon as the call and every earlier one have finished; after a
  follow-up item it announces the calls up to the next one.
- Streaming may receive client-visible output interleaved with gateway calls. In that
  case `engine/agent_turn.rs::execute_round_output` executes the same `GatewayScheduler`
  concurrently while `gateway/lifecycle.rs::relay_round_events` walks the output items in
  order: each item's synthetic gateway lifecycle is presented as soon as its call finished
  (a call not announced up front is announced just before), followed by that item's
  deferred upstream frames through `StreamRelay::release_deferred` with `Release::Item`,
  which presents them at the item's `public_item_index`. Concurrency and wire ordering
  are therefore separate concerns.
- `public_output_items` is the public projection: custom function calls become
  `custom_tool_call`; gateway-owned internal function calls become their handler's
  `web_search_call`/`mcp_call` output; client-owned function calls remain function
  calls. The original gateway function calls and `function_call_output` results are
  retained separately for continuation persistence.

The round decision remains in `engine.rs`, after gateway execution:

| Decision | Condition and state transition |
|---|---|
| `RequiresClientAction` | At least one synchronous client-owned call exists. Any gateway calls from the mixed round have already executed; their internal calls/results are recorded before returning. |
| `Done` | No gateway result and no client-owned call remains, or only async client calls remain on the last permitted round or after two consecutive async-only continuations. Finalize accumulated output and usage; a multi-agent turn finishes with the calls pending. |
| `Continue` | Gateway calls ran, or the only client calls are async (`ClientCalls`), and round budget remains. Append the upstream output plus gateway results, set `tool_choice: auto`, and infer again; pending async calls stay in the input, and `tool/async_execution.rs` adds a note after each one in the upstream copy. |
| `Incomplete` | Gateway calls ran on the tenth round. Record the final calls/results and return `status: incomplete` instead of leaving a dangling call. |

`parallel_tool_calls` is an upstream model-generation preference, not a gateway
scheduler switch. It is forwarded to vLLM for all supported declaration mixtures and
defaults to `false` when omitted. Whatever calls the model emits are executed under
the per-round execution permit limit and each handler's same-tool safety policy.

#### `messages_context.rs` / `messages_loop.rs` / `messages_request.rs` / `messages_stream.rs` / `messages_tools.rs`

A **parallel, independent implementation** of the same shape of loop for the Anthropic
Messages API. `messages_stream.rs`'s own header comment describes it as "structurally
the Anthropic-native analogue of `GatewayStreamAccumulator`, kept deliberately parallel
for a future consolidation" — it never touches `RequestPayload`/`ResponsePayload`/
`AgentPipeline`/`ResponseAccumulator`/`GatewayStreamAccumulator`/`TranslationDispatcher`, operating
directly on Anthropic-shaped JSON. The two loops share only the protocol-neutral
pieces: typed gateway bindings, `GatewaySchedulerPolicy`, and `types::messages::tool_seam`. The round/timeout
constants (`MAX_GATEWAY_TOOL_ROUNDS`, `GATEWAY_TOOL_TIMEOUT`) are duplicated and
manually kept in sync with the Responses-side ones rather than shared — a known seam,
not an oversight, per the future-consolidation note.

The JSON and SSE loops read a model turn differently but use `messages_round.rs`
for the completed round's decision: continue with gateway-executed tools, return
client-executed function calls, or finish. That policy also owns the client-call
`end_turn` correction. SSE ingestion must establish completion before consulting
it; partial or invalid rounds never execute tools. Text still streams during
ingestion rather than waiting for the decision.

Both paths dispatch `types/messages/tool_call.rs`'s `MessagesToolCall` through
`messages_tools.rs`'s `execute_gateway_calls`, so admission, execution, and the
fed-back `tool_result`s cannot differ between them. Streamed arguments are parsed
once when consuming the buffered block; history and execution share that input,
with a typed error retained independently of the legacy empty history fallback.
Opaque history fields and block order are preserved. Budget admission and
history/tool-choice mutation keep their existing owners.

Calls are admitted sequentially in model order before any of them starts, then execute through
the same permit-acquisition path as Responses. Each round has a fresh window of
`tools.max_concurrent_gateway_calls` executions (default 5). Calls acquire their
binding's same-tool exclusion before a round slot, then a materialization permit from
the policy shared by the `ExecutionContext`. Aliases resolve to the same binding and
therefore share its exclusion. The 60-second execution timeout starts after all three
permits are acquired; permit waits are not part of that timeout. Refused calls and
missing handlers return error `tool_result`s without acquiring execution capacity.
Results remain in model call order even when later calls finish first, and a completed
call immediately releases capacity for a waiting call. The owning loop polls these
futures directly; dropping it releases active and partially acquired permits and drops
pending executions. Remote side effects are not undone by local cancellation.

This bounds active execution, not the number of waiting call futures, accumulated
content, or retained tool outputs. Those Messages resource limits remain tracked in
[#313](https://github.com/vllm-project/agentic-api/issues/313).

A native `web_search` declaration's `max_uses` is a request-wide budget of
searches, not of calls. The normalized tool sent upstream lets one call batch several
`queries`, so a call is charged for every query the handler would run, counted by the
handler's own argument parser. A call the remaining budget cannot cover is refused
whole with an error `tool_result` and leaves the budget untouched, so a later call that
fits still runs. A call whose arguments cannot be parsed performs no search and is not
charged.

A native `web_fetch` declaration is handled the same way (`tool/web_fetch`,
#408): the upstream sees an ordinary `web_fetch` function tool with a single `url`
argument, and the gateway executes the call. The Messages adapter
(`messages_request.rs`) judges the Anthropic-specific settings — tool version,
`citations`, cache settings, `allowed_callers`, `max_uses` — reads the shared parameters
into `WebFetchToolParam`, and routes them through `WebFetchHandler::validate` and
`normalize`; the registry seam reads a declaration through the same parser.
`GatewayExecutors::require_web_fetch` is the one owner of the operator switch, used by
registry construction and by `count_tokens` alike. Ownership is request-scoped and
derived from the registry (`messages_tools::request_gateway_map`): the `web_fetch` name
is gateway-owned only when the request-scoped registry bound it to the gateway executor,
which only a native declaration does, so a client function that happens to be named
`web_fetch` stays client-owned. The fetch budget is separate from the search budget and
counted in fetches; every admitted call is charged whatever its outcome, and a call whose
arguments carry no URL is not. Two rules live in the loop rather than the handler because
only the loop holds the conversation: the budget, and the Anthropic rule that a URL must
already appear in a user message or a `tool_result` (including hidden `web_search`
results) before it can be fetched. The handler owns everything else — URL admission, the
address policy and DNS pinning applied to every redirect hop, domain filtering,
HTML-to-text extraction, the content limit, and the `max_concurrent_gateway_calls`
ceiling on fetches in flight — and answers documented failures in the
`web_fetch_tool_result_error` shape as a `ToolOutput` with a failure status, which the
loop reports as `is_error` without reading the output. Retrieval sits behind the
crate-private `WebFetchBackend` trait; the built-in HTTP backend is the default.

Both tools are executed in every version `tool_seam` lists (`NATIVE_WEB_SEARCH_VERSIONS`,
`NATIVE_WEB_FETCH_VERSIONS`; #419). Each `NativeToolVersion` records what its version
defines beyond the basic one, as Anthropic documents it, and the adapter reads those
fields rather than comparing version strings. A dynamic-filtering version's default
caller is code execution, so its declaration is accepted only when `allowed_callers`
includes `"direct"`, and is then rewritten into the basic version's function tool. The
fetcher keeps no cache and the gateway never calls a tool from code execution, so
`use_cache` and `response_inclusion` are accepted on the versions that define them; on
every other version of either tool they are refused. The registry seam
(`tool::declaration::web_search_config`) reads the shared settings of every listed
version, so a later version keeps its domain lists and location, and the rewrite itself
reports whether it changed a declaration, which is what `count_tokens` uses to decide
whether to forward the rewritten body.

Both loops take a `MessagesRequestContext` (`messages_context.rs`), the per-request
type that replaced a bare `serde_json::Value` at that boundary. It holds two views of
one request: typed fields for reading `tools`/`stream`/`model`, the current
`MessagesToolChoice`, and the raw JSON body that is actually forwarded upstream. The raw body is deliberately *not*
re-serialized from the typed view — `ContentBlock` catches unmodeled block types in
`#[serde(other)] Unknown` and models only the fields the gateway reads, so a typed
round-trip would drop `cache_control` and `is_error` and collapse `image`/
`redacted_thinking` into `{"type":"unknown"}`. The context owns every mutation the
loops make to that body (`force_stream`, `append_round`) and the native web-search
budget, so the two views cannot drift apart uncontrolled; `messages` and `system` are
reachable only through the raw body, never the typed view.

`append_round` also changes a fulfilled forced `tool_choice` (`any`, or `tool`
matching a returned gateway call result) to `auto`. The first round retains the
client's selector; later rounds can answer from the tool result or choose another
tool. Parallel-use settings and extension fields remain intact. Rounds containing
client-executed function tools return before this mutation, and Messages does not
persist this state.

`MessagesToolChoice` models `auto`, `any`, `tool` with a non-empty name, and
`none` as an exhaustive enum. It retains parallel-use settings and flattened
extension fields. The context performs fulfillment checks and transitions using
that enum, then serializes the changed selector into the raw body. Malformed
gateway-tool requests return HTTP 400 before inference; a parse failure cannot
bypass validation by falling back to the proxy. Requests without gateway tools
retain the transparent proxy contract and upstream validation.

vLLM can label a completed, explicitly named tool call `end_turn`. The shared
request context accepts that stop only when the selected gateway tool appears in
the round. Streaming additionally requires `message_stop`; client-executed
function tools and truncated rounds remain terminal. A completed client-executed
`tool_use` is surfaced with public `stop_reason: tool_use`, correcting vLLM's
`end_turn` in JSON and the final SSE `message_delta`. Mixed rounds hide gateway
calls and await the client's output. Token limits, other stop reasons and streams
without a completed round keep their original terminal semantics.

Hide-the-call covers every terminal round, not only a mixed one. A round can end while a
gateway `tool_use` is present whenever the stop reason is not a tool-call stop — a
`max_tokens` truncation mid-call, or an `end_turn` the context does not accept — and that
call is never executed. `messages_loop.rs`'s `deliver` strips gateway-owned `tool_use`
blocks from the returned message, matching the streaming accumulator, which suppresses them
on every round. The assistant turn fed back to the model still carries the call.

Each round's `usage` is folded into `messages_usage.rs`'s `MessagesUsageTotals`, so when
hidden gateway rounds ran, the returned message (JSON) and the terminal `message_delta` (SSE)
report the saturating sum of the Anthropic token counters across every round rather than the
final round alone. A streamed round counts its `message_start.usage` overlaid by its
`message_delta.usage`. Counters no round reported stay absent, the remaining `usage` fields
pass through from the final round, a single-round turn is returned unchanged, and a final
round that omits `usage` still reports the hidden rounds' counters.

### `storage/` — persistence

- **`pool.rs`** — `DbPool = sqlx::Pool<sqlx::Any>`, driver-agnostic across SQLite and
  Postgres. `create_pool`/`create_pool_with_schema` and friends build and tune it
  (WAL mode + busy-timeout retry on SQLite, statement/lock timeouts on Postgres).
- **`backend.rs`** — `DatabaseBackend` (Postgres/Sqlite/Other) detection from a
  connection URL, plus URL redaction for safe logging.
- **`schema.rs`** — migrations and readiness (`PoolWithSchema::ensure_schema_ready`),
  including a path for a supervisor-managed schema that skips running migrations
  itself and just verifies compatibility. Readiness probes live in `schema/readiness.rs`.
- **`conversation/api.rs`** — tenant-scoped conversation and item CRUD. Item ownership is checked through
  the conversation so Responses-persisted items are visible too. Item writes share `lock_in_tx` with
  turn persistence; a monotonic conversation revision makes deletions invalidate active snapshots.
  Removal detaches items rather than destroying history referenced by stored responses.
- **`models/`** — raw `sqlx::FromRow` row structs per table (`Conversation`, `Item`,
  `Response`) plus their raw, transaction-aware SQL functions (`create_in_tx`, `get`,
  `lock_in_tx`, ...). This is the literal DB row shape: JSON columns are still strings
  here.
- **`types/`** — the conversion layer from those raw rows into business types, via
  `From`/`TryFrom` impls: `ConversationData`/`ConversationSnapshot`, `ResponseData`/
  `ResponseMetadata` (parses the JSON metadata column into a typed struct, including an optional
  terminal `ResponsePayload` snapshot for GET retrieval and a versioned multi-agent
  tree checkpoint for continuation), and `StorageError`. The storage-specific `TryFrom<&Item>` implementation decodes
  rows into the shared `types/io::InOutItem`, preserving input/output identity and
  private origin markers. Store rehydration uses this conversion and fails
  if a row cannot be decoded; response rehydration also rejects missing referenced rows.
  It must not silently omit malformed reasoning from a continuation.
  `TryFrom<Response>` rejects malformed history references and effective metadata;
  only SQL NULL keeps the legacy empty/default behavior. Versioned conversation
  metadata lookup also rejects a missing or foreign captured response. Storage
  errors omit parser diagnostics that could echo sensitive persisted fields.
  `InOutItem::into_input_items` turns a full
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
  `ResponsesInput::normalized_model_input`: the latest compaction
  checkpoint is converted to an assistant summary and supersedes older context, while compaction triggers and MCP
  list-tools records and public collaboration items are removed. In particular, MCP
  list-tools remains available long enough for the registry to remember which server labels have already been listed,
  but it is never serialized to vLLM.
- **`conversation.rs`, `response.rs`** — `ConversationStore` and `ResponseStore`: the
  CRUD-with-transactions layer (`create`, `get`, `get_or_create`, `rehydrate[_snapshot]`,
  `persist`/`persist_if_version` — each transactional, via `pool.begin()` /
  `tx.commit()`). **These are not to be called outside `executor/` and `storage/`
  themselves.** The only sanctioned callers are `executor/modes/conversation.rs` and
  `executor/modes/response.rs`, described above. (Integration tests and benches import
  them directly for fixtures — that's expected and fine; production code paths should
  not.)

Stored Responses snapshots are written in the same transaction as response history. Retrieval goes through
`ResponseHandler::retrieve`, independently of upstream availability. Continuation checkpoints omit the
snapshot to avoid retaining a duplicate response; they continue to use canonical history and effective
settings. Legacy history-only records remain usable for continuation, but GET retrieval reports a conflict
rather than fabricating status, usage, or output.

### `tool/` — the tool framework

Wire shapes for tool declarations live in `types::tools` (see above); this module owns
the behavioral layer — routing, handler traits, normalization, and execution.

Every supported tool, whether client-owned or gateway-owned, implements `ToolHandler`.
That trait is the authority for validating a public declaration and normalizing it to
the fixed `FunctionTool` format understood by the upstream model. The outbound path is:

```text
public ResponsesTool declaration (read as a ToolDeclaration)
        │
        ▼
RequestPayload::to_upstream_request
        │
        ├─▶ ToolDeclaration::validate ──────────▶ ToolHandler::validate
        └─▶ ToolDeclaration::to_function_tools ─▶ ToolHandler::normalize
                                                     │
                                                     ▼
                                      canonical UpstreamTool::Function
```

`RequestPayload::to_upstream_request` is the only request-level seam that prepares
tools for vLLM. New callers must use it rather than rebuilding function schemas or
normalizing declarations in the executor. Declared placeholders that are not yet
supported, currently file search, produce no upstream function declaration until they
have a complete handler and execution path. Code interpreter has a fixed
normalization contract in every build. Requests that declare it fail before
inference unless the binary includes the `embedded-code-interpreter` feature,
the operator enables it, and Eryx runtime readiness succeeds.

| Component | Responsibility |
| --- | --- |
| `ToolHandler` | Validate one supported public tool declaration and normalize it into one or more canonical model-visible `FunctionTool`s. |
| `ToolRegistry` | Provide the request's read-only name lookup for all available tools, including tool type, client/gateway ownership, server label, and any typed gateway binding. |
| Client `ToolTranslator` | Convert a validated canonical function call back to that client-owned tool's public output shape and SSE lifecycle. |
| `GatewayExecutor` and `gateway.rs` | Execute gateway-owned calls and map their start, completion, failure, result, and public output lifecycle. |
| `GatewayStreamAccumulator` | Project gateway and upstream lifecycle frames into one continuous, correctly indexed and sequenced client stream. |

- **`declaration.rs`** — the protocol-neutral `ToolDeclaration`, the one concrete type the
  registry and the declaration helpers accept, and each API's conversion into it at its
  adapter boundary: `responses_declarations` (and `From<ResponsesTool>`) for the Responses
  wire enum, `registry_tools` for the Messages `ToolParam` blocks. What MCP discovery
  records on a declaration is propagated back into a Responses request's wire tools by
  `record_discovered_mcp_tools`, explicitly, where the request needs it for upstream use
  and persistence.
- **`normalize.rs`** — `validate()`, `tool_type()`, and `to_function_tools()` on
  `ToolDeclaration`. These are the declaration-level validation and normalization entry
  points used by `RequestPayload::to_upstream_request` on the converted declarations. Each supported variant's
  policy belongs to its corresponding `ToolHandler`: `FunctionHandler`,
  `ToolSearchHandler`, `McpHandler`, `WebSearchHandler`, `WebFetchHandler`,
  `CodexNamespaceHandler`, `CustomHandler`, or `CodeInterpreterHandler`. Web search's fixed canonical builder
  is shared with `WebSearchHandler::normalize`; it remains one schema even though it has no
  per-declaration normalization state. The method name is plural because namespace and
  MCP declarations may expand to several model-visible function tools.
  `FileSearch` remains an unsupported placeholder and normalizes to nothing. The
  code interpreter normalizes to one fixed function contract in every build, while
  request validation rejects unavailable runtimes before upstream inference.
  Feature-enabled builds bind the ready private `CodeInterpreterExecutor` to the provider.
  `ToolDeclaration::declares_async` and `declares_unsupported_async` answer which
  declarations make a tool async and which carry `async` where it is not accepted;
  `GatewayExecutors::validate_declarations` rejects the latter with its declared index.
- **`async_execution.rs`** — the model-visible hints for async client tools. A vLLM-served
  model stops at a call, so the gateway continues the response with the call pending.
  `FunctionHandler`, `CustomHandler`, and `CodexNamespaceHandler` normalization append
  the background-execution suffix to async tool descriptions, and
  `RequestPayload::to_upstream_request` calls `lower_async_calls`, which removes the public
  `async` marker from replayed calls and adds a `developer` note after each pending one.
  The texts equal the `upstream_hints` fixture, whose effect the recorder's `--sample` runs measured.
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
  lifecycle projection (and, for a tool that knows its result when planning, the
  `PlannedResult` recorded instead of executing), and `public_output()` shapes the
  completed/failed client-visible item. No hook builds model input: the gateway records
  the result's items, and the upstream projection lowers them.
  - **Client-owned** tools implement `ToolHandler` and have a translator association:
    see `function.rs` (`FunctionHandler`), `custom.rs` (`CustomHandler`), `codex.rs`
    (`CodexNamespaceHandler`), and `tool_search.rs` (`ToolSearchHandler`, for
    `execution: "client"`; `tool_search/history.rs` loads the definitions replayed search
    outputs carry, through `tool_search/definitions.rs`, and checks the search rules the
    pending-call ledger cannot see). Their calls are returned for the client to resolve; the
    gateway does not execute them.
  - **Gateway-owned / built-in** tools implement both traits: see `web_search/mod.rs`
    (`WebSearchHandler`, backed by the configured `WebSearchProvider` in `web_search/you.rs`,
    `web_search/brave.rs`, `web_search/tavily.rs`, `web_search/searxng.rs`, or `web_search/serply.rs`) and
    `mcp/handler.rs` (`McpHandler`, backed by `mcp/client.rs`'s MCP protocol client and `mcp/pool.rs`'s connection
    pool).
    `web_fetch/mod.rs` (`WebFetchHandler`, Messages-only, backed by a `WebFetchBackend` —
    the built-in `web_fetch/http.rs` fetcher — with `web_fetch/policy.rs` for URL and
    address admission, `web_fetch/extract.rs` for HTML-to-text extraction, and the shared
    `domain_policy.rs` for the domain lists both web tools declare) follows the
    same pattern, as does `tool_search/hosted.rs` (`HostedToolSearchExecutor`, for hosted
    search). They have no client translator association because the gateway owns
    their execution and public lifecycle.
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
      tools: &mut [ToolDeclaration],
      executors: &mut GatewayExecutors,
  ) -> Result<Self, ToolError>
  ```
  is the stable entry point every caller uses to build a registry for a request —
  Responses passes the declarations `responses_declarations` converted from its
  `ResponsesTool`s and records discovery back with `record_discovered_mcp_tools`,
  Messages the `ToolDeclaration`s from `registry_tools` — **its signature should not
  change**. It resolves namespace
  members, inserts one entry per declared/discovered tool, and for `Mcp`/`WebSearch`
  pulls the actual executor from `GatewayExecutors` (discovering live MCP tools via
  `tools/list` in the process). The Messages dispatcher resolves the normalized call
  name to a binding and acquires the shared scheduler policy's permits before execution.
  The Responses `GatewayScheduler` resolves that binding into one call plan so
  execution, self-exclusion, item position, and lifecycle hooks cannot drift apart.
  `ToolRegistry::dispatch(call)` remains a direct, unscheduled routing helper.
  After construction, consumers query this catalog for
  classification, ownership, and gateway bindings. `upstream.rs` snapshots its
  classification facts into a per-round `TranslationContext`, which translators use
  when restoring client-owned tool calls.

  MCP discovery history is also request-scoped registry state:
  `mcp_list_tools_items: IndexMap<String, Vec<McpListTools>>` groups records by
  `server_label`. Registry construction accepts the response's discovery history
  before materialization, so restored children can reuse the root's successful
  listing even when their forked message context omits it. Current declarations
  and allowlists still validate the tools exposed for execution. Reusable
  retained listings must cover an explicit `allowed_tools` set. Removing that
  restriction cannot prove a public listing is complete, so request-declared
  servers rediscover their catalog; configured servers can use their authoritative
  discovery cache. Expanding a restriction beyond the listing also rediscovers.
  Preparation appends newly discovered listings to the parent request's enriched input,
  so subsequent restored agents reuse them without a separate discovery history. Each
  agent's model context still comes from its own stored history. This also applies when
  an MCP declaration is first added on a later turn. Discovery metadata
  does not activate an idle agent when the client returns its full history.
  Stateless agent restoration reconstructs the parent's loaded tool definitions at
  each spawn through `ToolSearchState`, so `fork_turns: none` removes messages without
  discarding inherited tool availability on a later client-tool continuation.
  Equivalent successful listings are suppressed in JSON and SSE; failed or changed discovery
  remains visible. Streaming clears the lifecycle records after their first emission.
  Returned public `mcp_call` history uses the common model-input conversion to
  recover its function-call/result pair using the current discovered binding,
  with retained listing names as a fallback. Alias assignment precedes request
  allowlist filtering so name collisions stay stable across continuations.
  Public records do not retain private inference call IDs; their public item ID
  identifies the reconstructed pair without changing client-owned call IDs.
- **`executors.rs`** — `GatewayExecutors`, a shared registry built once at startup and
  reused across requests. Configured MCP servers are the only lazily connected slot:
  clients and discovered handlers are created on demand and cached by `server_label`
  across requests; request-declared handlers are reused only within a request.
  With a retained successful listing, their connection is established only when
  a tool is invoked, so a passive continuation neither reconnects nor lists tools.
  `McpHandler` distinguishes spec-only, connected, and lazy connection states;
  members of a restored server share its initialization cell. Lazy connection and
  fresh discovery select the same trace-propagation policy: request-declared
  entries use `McpClientPool::from_request_entries` without propagated trace
  context or baggage; operator-configured entries use `from_config` with propagation.
  Fresh discovery remains eager. Client-returned schemas stay request-scoped; only actual remote
  discovery populates the configured-server discovery cache. The shared
  `WebSearchHandler` and the opt-in `CodeInterpreterExecutor` are built once at
  startup; the code interpreter is backed by an embedded Eryx sandbox, enabled
  by configuration, and the default config is disabled. These executors bind
  into the existing gateway scheduler, not a
  second tool loop. Client-owned
  tools (`function`, `custom`, `shell`, `namespace`) never touch this file; their registry
  entries are inserted with `ToolOwnership::Client` and no `GatewayExecutors`
  involvement. Shell is not gateway-executed: `ShellHandler` deliberately does
  not implement `GatewayExecutor`, so declaring a shell tool never grants the
  gateway permission to execute arbitrary commands (PR #264 removed the optional
  executor; RFC #352 proposes a gateway-executed shell backend as future work).

Shell item history is preserved publicly in storage. At the inference boundary,
stored shell calls lower into `shell` function calls through the standard
output-to-input conversion, and typed input conversions lower submitted shell
calls and outputs into their function-history forms. Call and output share one
`call_id`, just as declarations and explicit shell selectors are normalized to
their function representations. The client execution contract is described in
`docs/design/shell-tool.md`.

**To add a new tool type:**
1. Implement `ToolHandler`, including its typed `ToolParams`, for it.
2. If it's gateway-executed, also declare typed `GatewayExecutor::ExecutionParams`
   and implement `execute` — see `web_search/mod.rs`/`mcp/handler.rs`.
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
| Change request admission (authentication, rate limits, or gateway-raised errors) | The layer order in `app.rs`'s `build_router_with_auth`, then `auth.rs`, `rate_limit.rs`, or `error_response.rs`; startup settings in `server_settings.rs` |
| Support a new upstream SSE event | `events/types.rs` → `events/normalize.rs` → `executor/accumulator/` → `executor/translate/` when the event needs public tool-shape translation |
| Add a new tool type | `tool/handler.rs` impl(s) → `tool/declaration.rs` (a `ToolDeclaration` kind and its view) → `tool/normalize.rs` → `tool/registry.rs` → `executor/translate/client.rs` for client-executed function shapes → `tool/executors.rs` for lazy gateway setup |
| Change gateway-round concurrency or lifecycle ordering | `executor/gateway.rs` (`GatewayScheduler`/event plans) + `executor/engine.rs` (round decision/ordered streaming) + `tool/ownership.rs` (typed binding and same-tool safety) |
| Change agent admission, mailbox wakeups or client continuation routing | `executor/engine/multi_agent/` + `executor/multi_agent/`; keep tree persistence in the existing mode handlers |
| Change single-agent steering admission or handoff | `handler/websocket/responses/control.rs` + `executor/session.rs` + `executor/engine/{execute,retained}.rs`; retain canonical input and publication ownership in core |
| Change the safe steering boundary | Existing completion validation in `executor/accumulator/{active,slot}.rs` + `executor/pipeline.rs` + `executor/engine/agent_turn.rs`; preserve one ingestion/finalization path and drain started tools |
| Change client streaming order, buffering, or backpressure | `executor/relay.rs` and `executor/relay/`; keep parsing in `events/` and response assembly in `executor/accumulator/` |
| Move streaming ingestion to a worker | Benchmark the equivalent inline and worker paths under [#245](https://github.com/vllm-project/agentic-api/issues/245) before changing executor placement |
| Feed response output into the next inference round | `types/io/output.rs::OutputItem::to_input_item`; use `executor/gateway.rs::append_output_items_to_input` only to append those converted items |
| Change continuation history visibility | `types/io/item.rs::InOutItem::into_input_items` → `types/io/output.rs::to_input_item` (preservation) → `types/io/input.rs::model_input` (upstream visibility; search items via `types/io/input/search_history.rs`) |
| Add a CRUD operation beyond persist/rehydrate | `executor/modes/conversation.rs` or `modes/response.rs`, backed by `storage/conversation.rs` / `storage/response.rs` |
| Change how output items are assembled from a stream | `executor/accumulator/` — extend the typed slot and transition matches through the existing `RoundIngestion` path |
| Add a new Responses/Messages wire field | `types/io/` or `types/messages/` — shape only, no behavior |

## Further reading

- [AGENTS.md](AGENTS.md) — module boundaries, lint/format rules, commit and PR conventions
- [TERMINOLOGY.md](TERMINOLOGY.md) — normative vocabulary for API/state/tool/streaming concepts
- [ROADMAP.md](ROADMAP.md) — project direction and near-term focus
- [docs/adr/](docs/adr/) — architecture decision records
- [docs/design/](docs/design/) — as-built design docs (tool framework, core public API, MCP integration, Codex integration)
