# ADR-03 — Layered Crate Architecture

> **Status:** Draft
> **Related:** [ADR-01 — Core Architecture](ADR-01_core.md), [ADR-02 — Response Store](ADR-02_response_store.md), [PR #24](https://github.com/vllm-project/agentic-api/pull/24), [PR #27](https://github.com/vllm-project/agentic-api/pull/27)

---

## Intention

This ADR settles:

1. How agentic-api is structured as a crate workspace (three layers)
2. How it integrates with Praxis as the primary gateway
3. How the agentic loop is composed from public functions that can be customized

---

## Context

Agentic-api is the orchestration core for the vLLM Responses API. It manages the agentic loop: conversation rehydration, inference calling, tool dispatch, response persistence. The question is how it relates to the gateway proxy that sits in front of it.

Note: ADR-01 decided on Python as the project language. The project has since transitioned to Rust ([PR #23](https://github.com/vllm-project/agentic-api/pull/23)), and once accepted, this ADR will supersede ADR-01's language decision (D3).

### Praxis as the gateway

Praxis is the primary gateway for agentic-api. It is a Rust-native, early-stage proxy with a co-development opportunity — agentic-api is an early adopter that validates Praxis's integration model with a real agentic workload.

The out-of-the-box version of agentic-api ships a Praxis filter that composes the agentic loop by calling `agentic-core` public functions. The key requirement is that consumers can build their own filter with a different loop — adding, removing, or reordering steps — by calling the same public functions (requires recompilation, not dynamic).

### The integration question

One integration model has been proposed: decompose the agentic loop into **Praxis re-entrant filter chains** where each step is a Praxis-specific filter. An alternative is to implement the loop steps as plain Rust public functions that a Praxis filter calls to compose the loop. The functions are testable without Praxis and usable in standalone mode. Because they are plain Rust with no gateway-specific API, they could support other gateways in the future.

---

## Decision

### Three-layer crate architecture

```
┌─────────────────────────────────────────────┐
│  Layer 3: Gateway adapter (Praxis)          │
│                                             │
│  ┌─────────────────────────┐                 │
│  │  agentic-praxis filter  │                 │
│  │  (composes the loop)    │                 │
│  └─────────────────────────┘                 │
├─────────────────────────────────────────────┤
│  Layer 2: HTTP API (agentic-server / axum)  │
│                                             │
│  POST /v1/responses → calls into core       │
│  SSE streaming, health checks, CLI          │
├─────────────────────────────────────────────┤
│  Layer 1: Core library                      │
│  (pure Rust, no framework dependency)       │
│                                             │
│  Executor (loop state machine)              │
│  Conversation manager                       │
│  Response store (trait-based backends)      │
│  Tool registry + dispatch                   │
│  MCP client                                 │
│  Inference caller                           │
│  Response assembler                         │
│                                             │
│  No axum. No Praxis. No framework.          │
│  Just async Rust + traits.                  │
└─────────────────────────────────────────────┘
```

### Key decisions

| # | Decision | Status |
|---|----------|--------|
| D1 | Core orchestration logic is a Rust library crate (`agentic-core`) that exposes each loop step as a public function — plain Rust, no gateway-specific API | Proposed |
| D2 | The agentic loop is composed by calling `agentic-core` public functions — either inside a single filter (Model A) or across per-step filters with Praxis re-entrant chains (Model B); both use the same functions | Proposed |
| D3 | Response store, conversation manager, tool registry, and MCP client are implemented natively in Rust within `agentic-core` | Proposed |
| D4 | Praxis integrates via `agentic-praxis`, a filter that composes the default loop by calling `agentic-core` public functions — deployed either as a backend service or as an in-process library | Proposed |
| D5 | Standalone mode (axum binary) is first-class — same core functions, different hosting | Proposed |

---

## Crate Structure

```
agentic-api/
  Cargo.toml              # [workspace]

  crates/
    agentic-core/          # Layer 1: pure library
      Cargo.toml           # [lib], deps: tokio, reqwest, serde, sqlx
      src/
        lib.rs
        executor.rs        # Loop state machine
        store.rs           # Response store (trait + impls)
        conversation.rs    # Conversation manager
        inference.rs       # vLLM proxy / inference caller
        tools/
          mod.rs           # Tool registry + dispatch
          mcp.rs           # MCP client (stdio/SSE)
          builtin.rs       # web_search, file_search, code_interpreter
          host.rs          # Sandboxed host tools

    agentic-server/        # Layer 2: axum standalone binary
      Cargo.toml           # depends on agentic-core
      src/
        main.rs            # CLI, axum server, vLLM subprocess mgmt

    agentic-praxis/        # Layer 3: Praxis adapter
      Cargo.toml           # depends on agentic-core + praxis
      src/
        lib.rs             # Single filter: receive request → core → stream response
```

### Layer 1: `agentic-core`

The core crate exposes each step of the agentic loop as an individual public function. This allows consumers to compose steps with their own logic (e.g. rate limiting before tool invocation, custom guardrails between inference and response assembly).

```rust
// High-level: run the full loop in one call
pub async fn execute(
    request: ResponsesRequest,
    ctx: &ExecutionContext,
) -> Result<ResponseStream, Error>

// Individual loop steps — composable building blocks
pub async fn rehydrate_conversation(...) -> Result<Conversation, Error>
pub async fn call_inference(...) -> Result<InferenceResult, Error>
pub async fn dispatch_tools(...) -> Result<Vec<ToolResult>, Error>
pub async fn assemble_response(...) -> Result<Response, Error>
pub async fn persist_response(...) -> Result<(), Error>
```

`execute()` is a convenience that composes these steps with the default loop logic. Consumers who need fine-grained control (custom middleware between steps, per-step observability, conditional branching) call the individual functions directly. Each function can also be wrapped in its own gateway filter — a consumer who wants per-step filters can build them from these primitives without the core prescribing the decomposition.

Dependencies: `tokio`, `reqwest`, `serde`, `serde_json`, `sqlx`, `thiserror`. No server-side framework dependencies (`axum`, `praxis`, `tower`).

### Layer 2: `agentic-server`

Thin axum wrapper. Parses HTTP, calls `agentic_core::execute()`, streams the result. Owns the CLI (`clap`), vLLM subprocess management, and standalone server lifecycle. PR #24 will introduce the proxy logic, configuration, error handling, and CLI that form the basis of this layer.

### Layer 3: `agentic-praxis`

The default Praxis filter. Composes the agentic loop internally by calling `agentic-core` public functions. The out-of-the-box filter calls `execute()` for the standard loop. Consumers who need a custom loop (e.g. adding rate limiting before tool invocation, or inserting guardrails between inference and assembly) build their own filter that calls the individual step functions directly.

Praxis depends on `agentic-praxis` as a crate, which transitively brings in `agentic-core`:

```toml
# In Praxis's Cargo.toml or a downstream build
[dependencies]
agentic-praxis = "0.1"  # pulls in agentic-core automatically
```

agentic-api publishes releases on its own schedule. Praxis bumps the version when ready.

---

## Integration Models

### Praxis (production)

```
Client → Praxis (auth, rate-limit, routing) → agentic-server
                                                    │
                                                    ▼
                                              agentic-core
                                                    │
                                                    ▼
                                              vLLM / llm-d
```

Praxis sees agentic-api as an HTTP backend — the same way it sees vLLM. For stateful requests (`previous_response_id`, tools), Praxis routes to agentic-api. For stateless pass-through, Praxis routes directly to vLLM.

Alternatively, Praxis can link `agentic-praxis` as an in-process filter, eliminating the network hop while keeping the same core logic:

```
Client → Praxis (auth, rate-limit, routing, agentic-praxis filter) → vLLM
                                                │
                                          agentic-core
                                          (in-process)
```

Both modes use the same `agentic-core` code. The choice is a deployment decision, not an architecture decision.

### Standalone (development)

```
Client → agentic-server (axum) → vLLM (subprocess or external)
                │
          agentic-core
```

No gateway. Single binary. `agentic-api serve <model>` or `agentic-api --llm-api-base <url>`.

### Other gateways (future)

Praxis is what we start with. Because `agentic-core` functions are plain Rust with no gateway-specific API, supporting other gateways in the future is possible without changes to the core.

---

## Rationale

### Two composition models from the same primitives

Because `agentic-core` exposes each loop step as a public function, two composition models are possible — both use the same underlying code.

#### Model A: Single filter (default)

One Praxis filter composes the full loop internally by calling `agentic-core` functions:

```
Client → Praxis filters (auth, rate-limit, routing)
           → agentic-praxis filter
               calls: rehydrate → inference → tool_dispatch → assemble → persist
               (loop iteration is plain Rust control flow inside the filter)
               (inference step calls vLLM internally; response returns to client)
```

This is what `agentic-praxis` ships out of the box. The loop is self-contained, testable with `cargo test`, and works in standalone mode (axum) without Praxis.

#### Model B: Per-step filter chain ([praxis#354](https://github.com/praxis-proxy/praxis/issues/354))

Each `agentic-core` function is wrapped in its own Praxis filter. Praxis's re-entrant filter chains orchestrate the loop iteration:

```
Client → Praxis filters (auth, rate-limit, routing)
           → conversation_retrieval filter  (calls rehydrate_conversation)
           → pre_inference_guardrails filter
           → inference filter               (calls call_inference)
           → post_inference_guardrails filter
           → tool_dispatch filter           (calls dispatch_tools)
           → [re-enter chain if tool calls detected]
           → response_assembly filter       (calls assemble_response)
           → persistence filter             (calls persist_response)
           → response to client
           (inference filter calls vLLM internally)
```

Each filter wraps an `agentic-core` public function — the domain logic stays in `agentic-core`, the orchestration moves to Praxis's filter pipeline.

#### Comparison

| Concern | Model A (single filter) | Model B (per-step filters) |
|---------|------------------------|---------------------------|
| Loop control | Plain Rust control flow inside the filter | Praxis re-entrant filter chains |
| Per-step customization | Call individual functions with custom logic between them | Insert custom filters between steps in the chain |
| Observability | Explicit instrumentation inside the filter | Praxis provides per-filter metrics and tracing |
| Standalone mode | Works as-is (axum) | Requires Praxis |
| Praxis dependency | Runtime only (filter API) | Runtime + re-entrant chain support ([praxis#354](https://github.com/praxis-proxy/praxis/issues/354)) |
| Testing | `cargo test` on `agentic-core` functions directly | `cargo test` on functions + Praxis filter harness for integration |

Both models use the same `agentic-core` public functions. Model A is the starting point; Model B becomes available as Praxis's re-entrant chain support matures. The migration path is: wrap each function call in its own filter and move loop control to Praxis — no changes to `agentic-core`.

### Why three layers

- **Testability.** Core logic is tested without any HTTP server or gateway infrastructure.
- **Composability.** Consumers can customize the agentic loop by wiring individual functions differently — adding steps, reordering, or replacing functions.
- **Independent scaling.** As a service, agentic-api scales separately from the gateway. As an in-process filter, it shares the gateway's resources — the deployment choice is made at deploy time, not compile time.
- **Release independence.** Core and server ship on their own schedule. Adapters depend on the core crate version, not on the gateway's release cycle.

---

## Deployment Modes

```
MODE 1: Dev / standalone             MODE 2: Production (service)
──────────────────────               ────────────────────────────

  Client                               Client
    │                                    │
    ▼                                    ▼
  agentic-server (axum)                Praxis (Rust gateway)
    │  single binary                     │  auth, rate-limit,
    │  no gateway needed                 │  routing, guardrails
    ▼                                    ▼
  vLLM (subprocess                     agentic-server (Rust service)
    or external)                         │
                                         ▼
                                       vLLM / llm-d (fleet)


MODE 3: Production (in-process)
───────────────────────────────

  Client
    │
    ▼
  Praxis (with agentic-praxis filter linked)
    │  gateway filters + agentic core in one process
    ▼
  vLLM / llm-d (fleet)
```

---

## Impact on Existing PRs

### PR #24 — Rust proxy gateway

PR #24 will become the foundation of `agentic-core` and `agentic-server`. The proxy logic, configuration, error handling, and CLI it introduces will evolve into the layered crate structure. The standalone `serve` mode remains first-class in `agentic-server`. Benchmarks stay.

The workspace migration (flat crate → workspace with `crates/`) is a follow-up after PR #24 merges. PR #24 ships as-is — it's correct and complete for the current scope.

### PR #27 — Praxis filter-based architecture

PR #27 decomposes the agentic loop into multiple Praxis filters (`responses_proxy`, `agentic_loop`, `state_hydration`, `tool_dispatch`). If accepted, this ADR supersedes that approach: the loop stays as an explicit state machine in `agentic-core`, and the Praxis integration is a single thin filter in `agentic-praxis`.

If accepted, PR #27 should be closed in favor of this architecture.

---

## Implications

- **Workspace migration.** The current flat crate structure (`src/`) will migrate to a Cargo workspace with `crates/agentic-core`, `crates/agentic-server`, and `crates/agentic-praxis`. This happens after PR #24 merges as a separate refactoring PR.
- **Core API design.** The `agentic-core` public API (individual step functions, `execute()`, domain types) needs careful design — it's the contract that Praxis and any custom loop wiring depends on.
- **Praxis co-development.** We contribute `agentic-praxis` and work with the Praxis team to validate the integration model. agentic-api is an early adopter that exercises Praxis's capabilities with a real agentic workload.
- **State services.** Response store (ADR-02), conversation manager, and tool registry are implemented natively in Rust within `agentic-core`. No external Python services in the request path.

---

## Open Questions

1. **Praxis filter API stability.** The `HttpFilter` trait and `HttpFilterContext` API are young. How stable is the contract we build the adapter against? Mitigation: the adapter is thin (~50 lines), so API changes are cheap to absorb.

2. **Built-in tool implementation.** `web_search`, `file_search`, `code_interpreter` are listed as Rust-native. These are non-trivial to implement. What's the MVP subset? Likely: MCP client first (delegates to external tool servers), built-in tools later.

3. **Guardrails integration point.** Input guardrails can run in Praxis (pre-routing) or in agentic-api (post-hydration, with full conversation context). Output guardrails must run in agentic-api (per loop iteration). The split needs to be validated with the guardrails team.

4. **In-process vs service mode trade-offs.** Mode 2 (service) adds ~1ms per loop iteration but gives process isolation and independent scaling. Mode 3 (in-process) eliminates the hop but shares failure domains. Which is the default recommendation for production?
