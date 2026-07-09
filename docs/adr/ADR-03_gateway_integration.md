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

Note: ADR-01 decided on Python as the project language. PR #23 transitioned the project to Rust, superseding ADR-01's language decision (D3). This ADR assumes Rust throughout.

### Praxis as the gateway

Praxis is the primary gateway for agentic-api. It is a Rust-native, early-stage proxy with a co-development opportunity — agentic-api is an early adopter that validates Praxis's integration model with a real agentic workload.

The out-of-the-box version of agentic-api ships a set of Praxis filters (each implementing `HttpFilter`) that compose the agentic loop, with each filter backed by an `agentic-server-core` public function. Praxis's filter chain with branch support orchestrates the loop — including branching back on tool calls. In standalone mode (no gateway), `execute()` composes the same functions with plain Rust control flow. The key requirement is that consumers can customize the loop — adding, removing, or reordering filters — by calling the same public functions (requires recompilation, not dynamic).

### The integration question

The loop steps are implemented as plain Rust public functions in `agentic-server-core`. When integrated with Praxis, each function is wrapped in an `HttpFilter` and composed into a filter chain — using Praxis's branch chains to handle tool-call loops. In standalone mode, `execute()` composes the same functions with plain Rust control flow. Because the functions are plain Rust with no gateway-specific API, they could support other gateways in the future.

---

## Decision

### Three-layer crate architecture

```
┌─────────────────────────────────────────────┐
│  Layer 3: Gateway adapter (Praxis)          │
│                                             │
│  ┌─────────────────────────┐                 │
│  │  agentic-praxis         │                 │
│  │  (HttpFilter impls      │                 │
│  │   backed by core fns)   │                 │
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
| D1 | Core orchestration logic is a Rust library crate (`agentic-server-core`) that exposes each loop step as a public function — plain Rust, no gateway-specific API | Proposed |
| D2 | The agentic loop is composed by calling `agentic-server-core` public functions — in Praxis, each function is an `HttpFilter` in a filter chain with branch support; in standalone mode, `execute()` composes them with plain Rust control flow | Proposed |
| D3 | Response store, conversation manager, tool registry, and MCP client are implemented natively in Rust within `agentic-server-core` | Proposed |
| D4 | Praxis integrates via `agentic-praxis`, which wraps each `agentic-server-core` function in an `HttpFilter` — composed into a filter chain with branches for tool-call looping | Proposed |
| D5 | Standalone mode (axum binary) is first-class — same core functions, different hosting | Proposed |

---

## Crate Structure

```
agentic-api/
  Cargo.toml              # [workspace]

  crates/
    agentic-server-core/          # Layer 1: pure library
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
      Cargo.toml           # depends on agentic-server-core
      src/
        main.rs            # CLI, axum server, vLLM subprocess mgmt

    agentic-praxis/        # Layer 3: Praxis adapter
      Cargo.toml           # depends on agentic-server-core + praxis
      src/
        lib.rs             # HttpFilter impls: each wraps an agentic-server-core function
```

### Layer 1: `agentic-server-core`

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

The Praxis integration crate. Each `agentic-server-core` public function is wrapped in an `HttpFilter` implementation. The out-of-the-box configuration assembles these filters into a filter chain with branch support for tool-call looping. Consumers who need a custom loop (e.g. adding rate limiting before tool invocation, or inserting guardrails between inference and assembly) reconfigure the filter chain — adding, removing, or reordering filters.

Praxis depends on `agentic-praxis` as a crate, which transitively brings in `agentic-server-core`:

```toml
# In Praxis's Cargo.toml or a downstream build
[dependencies]
agentic-praxis = "0.1"  # pulls in agentic-server-core automatically
```

agentic-api publishes releases on its own schedule. Praxis bumps the version when ready.

---

## Integration Models

### Praxis (production)

```
Client → Praxis (auth, rate-limit, routing) → agentic-server
                                                    │
                                                    ▼
                                              agentic-server-core
                                                    │
                                                    ▼
                                              vLLM / llm-d
```

Praxis sees agentic-api as an HTTP backend — the same way it sees vLLM. For stateful requests (`previous_response_id`, tools), Praxis routes to agentic-api. For stateless pass-through, Praxis routes directly to vLLM.

Alternatively, Praxis can link `agentic-praxis` in-process, with each `agentic-server-core` function as an `HttpFilter` in the filter chain — eliminating the network hop while keeping the same core logic:

```
Client → Praxis (auth, rate-limit, routing, agentic-praxis filters) → vLLM
                                                │
                                          agentic-server-core
                                          (in-process)
```

Both modes use the same `agentic-server-core` code. The choice is a deployment decision, not an architecture decision.

### Standalone (development)

```
Client → agentic-server (axum) → vLLM (subprocess or external)
                │
          agentic-server-core
```

No gateway. Single binary. `agentic-api serve <model>` or `agentic-api --llm-api-base <url>`.

### Other gateways (future)

Praxis is what we start with. Because `agentic-server-core` functions are plain Rust with no gateway-specific API, supporting other gateways in the future is possible without changes to the core.

---

## Rationale

### Same functions, different orchestrators

`agentic-server-core` exposes each loop step as a public function. Who orchestrates the loop depends on the deployment context:

#### In Praxis

Each `agentic-server-core` function is wrapped in an `HttpFilter`. Praxis's filter chain orchestrates the loop — with branch chains handling tool-call re-entry.

**Filter implementations** — thin wrappers that delegate to `agentic-server-core`:

```rust
struct InferenceFilter { vllm_url: String }
impl HttpFilter for InferenceFilter {
    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction> {
        let result = agentic_core::call_inference(ctx.get("conversation"), &self.vllm_url).await?;
        ctx.set("inference_result", result);
        Ok(FilterAction::Continue)
    }
}

struct ToolDispatchFilter;
impl HttpFilter for ToolDispatchFilter {
    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction> {
        let result = ctx.get("inference_result");
        if result.has_tool_calls() {
            let tool_results = agentic_core::dispatch_tools(result.tool_calls()).await?;
            ctx.set("conversation", result.with_tool_results(tool_results));
            ctx.set_result("action", "loop");   // signal branch: tools called
        } else {
            ctx.set_result("action", "done");   // signal: no tools, continue
        }
        Ok(FilterAction::Continue)
    }
}
```

**YAML configuration** — Praxis orchestrates the loop declaratively:

```yaml
filter_chains:
  - name: agentic-loop
    filters:
      - filter: rehydrate
        name: rehydrate

      - filter: inference
        name: inference
        vllm_base_url: "http://localhost:8000"

      - filter: tool_dispatch
        name: tool_dispatch
        branch_chains:
          - name: tool-call-loop
            on_result:
              filter: tool_dispatch
              key: action
              result: loop              # branch when tools were called
            rejoin: inference           # re-enter at inference filter
            max_iterations: 10          # cap tool-call iterations

      - filter: assemble
        name: assemble

      - filter: persist
        name: persist
```

**The resulting flow:**

```
request arrives
  → rehydrate         (agentic_core::rehydrate_conversation)
  → inference          (agentic_core::call_inference → vLLM)
  → tool_dispatch      (agentic_core::dispatch_tools)
      ├─ action=loop → [branch: re-enter at inference, max 10 iterations]
      └─ action=done → continue
  → assemble           (agentic_core::assemble_response)
  → persist            (agentic_core::persist_response)
  → response to client
```

Each filter is a thin wrapper (~10 lines). All domain logic lives in `agentic-server-core`. Consumers customize the loop by editing YAML to add/remove/reorder/reconfigure filters that are already registered in the Praxis binary (for example, inserting a guardrail filter between inference and tool dispatch). Adding a brand-new custom filter implementation requires registering it in code and rebuilding the Praxis binary. This uses Praxis's filter chain and branch constructs natively ([praxis#354](https://github.com/praxis-proxy/praxis/issues/354)).

#### In standalone mode

`execute()` composes the same functions with plain Rust control flow — no gateway, no pipeline framework:

```
Client → agentic-server (axum)
           → agentic_core::execute()
               rehydrate → inference (→ vLLM) → tool_dispatch → assemble → persist
               (loop iteration is plain Rust async)
           → response to client
```

This is the development and community-friendly mode. Anyone can use `agentic-server-core` as a library without Praxis or any specific gateway.

#### Comparison

| Concern | Praxis (filter chain) | Standalone (execute) |
|---------|----------------------|----------------------|
| Loop control | Filter chain with branch support for re-entry | Plain Rust control flow |
| Customization | Reconfigure filter chain — add/remove/reorder filters | Call individual functions with custom logic between them |
| Observability | Praxis provides per-filter metrics and tracing | Explicit instrumentation |
| Gateway dependency | Praxis | None |
| Testing | `cargo test` on functions + Praxis filter chain for integration | `cargo test` on functions directly |

Both use the same `agentic-server-core` public functions — the domain logic is always in `agentic-server-core`, only the orchestrator differs.

### Why three layers

- **Testability.** Core logic is tested without any HTTP server or gateway infrastructure.
- **Composability.** Consumers can customize the agentic loop by wiring individual functions differently — adding steps, reordering, or replacing functions.
- **Independent scaling.** As a service, agentic-api scales separately from the gateway. As an in-process filter, it shares the gateway's resources. Runtime topology is a deployment decision, but in-process mode requires a Praxis build that includes `agentic-praxis` and its filter registrations.
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
  Praxis (with agentic-praxis filters linked)
    │  gateway filters + agentic core in one process
    ▼
  vLLM / llm-d (fleet)
```

---

## Impact on Existing PRs

### PR #24 — Rust proxy gateway

PR #24 will become the foundation of `agentic-server-core` and `agentic-server`. The proxy logic, configuration, error handling, and CLI it introduces will evolve into the layered crate structure. The standalone `serve` mode remains first-class in `agentic-server`. Benchmarks stay.

The workspace migration (flat crate → workspace with `crates/`) is a follow-up after PR #24 merges. PR #24 ships as-is — it's correct and complete for the current scope.

### PR #27 — Praxis filter-based architecture

PR #27 decomposes the agentic loop into multiple Praxis filters (`responses_proxy`, `agentic_loop`, `state_hydration`, `tool_dispatch`). This ADR aligns with that direction — each step is an `HttpFilter` in the filter chain — but with a key difference: the domain logic lives in `agentic-server-core` public functions, not in Praxis-specific filter implementations. PR #27's filter decomposition should be reworked so each filter delegates to an `agentic-server-core` function rather than implementing the logic directly.

---

## Implications

- **Workspace migration.** The current flat crate structure (`src/`) will migrate to a Cargo workspace with `crates/agentic-server-core`, `crates/agentic-server`, and `crates/agentic-praxis`. This happens after PR #24 merges as a separate refactoring PR.
- **Core API design.** The `agentic-server-core` public API (individual step functions, `execute()`, domain types) needs careful design — it's the contract that Praxis and any custom loop wiring depends on.
- **Praxis co-development.** We contribute `agentic-praxis` and work with the Praxis team to validate the integration model. agentic-api is an early adopter that exercises Praxis's capabilities with a real agentic workload.
- **State services.** Response store (ADR-02), conversation manager, and tool registry are implemented natively in Rust within `agentic-server-core`. No external Python services in the request path.

---

## Open Questions

1. **Praxis filter API stability.** The `HttpFilter` trait and `HttpFilterContext` API are young. How stable is the contract we build the adapter against? Mitigation: the adapter is thin (~50 lines), so API changes are cheap to absorb.

2. **Built-in tool implementation.** `web_search`, `file_search`, `code_interpreter` are listed as Rust-native. These are non-trivial to implement. What's the MVP subset? Likely: MCP client first (delegates to external tool servers), built-in tools later.

3. **Guardrails integration point.** Input guardrails can run in Praxis (pre-routing) or in agentic-api (post-hydration, with full conversation context). Output guardrails must run in agentic-api (per loop iteration). The split needs to be validated with the guardrails team.

4. **In-process vs service mode trade-offs.** Mode 2 (service) adds ~1ms per loop iteration but gives process isolation and independent scaling. Mode 3 (in-process) eliminates the hop but shares failure domains. Which is the default recommendation for production?

5. **Separate tools crate.** Tools currently live in `agentic-server-core`. If a tool implementation requires non-Rust-native dependencies (C bindings, external libraries), it may make sense to split tools into a separate `agentic-tools` crate to avoid polluting `agentic-server-core`'s dependency tree. Revisit when tool implementations land.
