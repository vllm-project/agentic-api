# ADR-03 — Gateway-Agnostic Layered Architecture

> **Status:** Draft
> **Related:** [ADR-01 — Core Architecture](ADR-01_core.md), [ADR-02 — Response Store](ADR-02_response_store.md), [PR #24](https://github.com/vllm-project/agentic-api/pull/24), [PR #27](https://github.com/vllm-project/agentic-api/pull/27)

---

## Intention

This ADR settles:

1. How agentic-api is structured as a crate workspace (three layers)
2. How it integrates with gateway proxies (Praxis, and potentially others)
3. Why the agentic loop is an explicit state machine, not proxy middleware

---

## Context

Agentic-api is the orchestration core for the vLLM Responses API. It manages the agentic loop: conversation rehydration, inference calling, tool dispatch, response persistence. The question is how it relates to the gateway proxy that sits in front of it.

Note: ADR-01 decided on Python as the project language. The project has since transitioned to Rust ([PR #23](https://github.com/vllm-project/agentic-api/pull/23)), and once accepted, this ADR will supersede ADR-01's language decision (D3).

### The gateway landscape

Multiple gateway options exist: Praxis (Rust, early-stage, co-development opportunity), Kong, Envoy, or no gateway at all (standalone mode for development). Coupling the orchestration core to any single gateway's plugin API creates lock-in and limits adoption.

### The integration question

One integration model has been proposed: decompose the agentic loop into **Praxis re-entrant filter chains**. This spreads tightly coupled domain logic (conversation rehydration, tool dispatch, loop control, response assembly) across loosely coupled filters. These components share state, depend on execution order, and interact through response semantics — they are the core transaction, not cross-cutting concerns. Encoding them as filters makes the system harder to reason about, harder to test (requires full filter harness), and couples the release cycle to Praxis. Additionally, re-entrant filter chains don't exist in Praxis yet ([praxis#354](https://github.com/praxis-proxy/praxis/issues/354)).

---

## Decision

### Three-layer crate architecture

```
┌─────────────────────────────────────────────┐
│  Layer 3: Thin adapters (one per gateway)   │
│                                             │
│  ┌───────┐ ┌──────┐ ┌─────┐                 │
│  │Praxis │ │ Kong │ │ ... │                 │
│  │ filter│ │plugin│ │     │                 │
│  └───────┘ └──────┘ └─────┘                 │
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
| D1 | Core orchestration logic is a framework-agnostic Rust library crate (`agentic-core`) with no HTTP server or gateway dependencies | Proposed |
| D2 | The agentic loop is an explicit async state machine in application code, not decomposed into proxy filter chains | Proposed |
| D3 | Response store, conversation manager, tool registry, and MCP client are implemented natively in Rust within `agentic-core` | Proposed |
| D4 | Praxis integrates via a single thin adapter filter that delegates to `agentic-core` — deployed either as a backend service (network routing) or as an in-process library (linked filter) | Proposed |
| D5 | Standalone mode (axum binary) is first-class, not an afterthought — same core code, different hosting | Proposed |
| D6 | Each gateway adapter is one thin integration point (one filter/plugin), not a decomposition of domain logic across many filters | Proposed |

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

`execute()` is a convenience that composes these steps with the default loop logic. Consumers who need fine-grained control (custom middleware between steps, per-step observability, conditional branching) call the individual functions directly.

Dependencies: `tokio`, `reqwest`, `serde`, `serde_json`, `sqlx`, `thiserror`. No server-side framework dependencies (`axum`, `praxis`, `tower`).

### Layer 2: `agentic-server`

Thin axum wrapper. Parses HTTP, calls `agentic_core::execute()`, streams the result. Owns the CLI (`clap`), vLLM subprocess management, and standalone server lifecycle. PR #24 will introduce the proxy logic, configuration, error handling, and CLI that form the basis of this layer.

### Layer 3: `agentic-praxis`

One Praxis filter. Receives the HTTP request from Praxis, extracts the body, calls `agentic_core::execute()`, streams the response back through Praxis. The filter has no domain logic — it's pure plumbing.

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
Client → Praxis (auth, rate-limit, routing) → agentic-api service
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

A Kong plugin, Envoy ext_proc adapter, or any other gateway integration follows the same pattern: thin adapter calling `agentic_core::execute()`. The core doesn't change.

---

## Rationale

### Single filter vs. decomposed filter chains

An alternative integration model is to decompose the agentic loop into multiple Praxis filters (one per concern: conversation management, tool dispatch, loop control, inference calling). Both approaches are viable; we prefer the single-filter model for agentic-api.

#### Decomposed filter chains

| Pros | Cons |
|------|------|
| Each concern is independently deployable and replaceable | Filters share state and depend on execution order — they are steps in a transaction, not independent cross-cutting concerns |
| Praxis controls the full pipeline, enabling fine-grained observability per filter | Testing individual filters requires the full Praxis filter harness rather than plain `cargo test` |
| Aligns with Praxis's long-term vision for re-entrant filter chains ([praxis#354](https://github.com/praxis-proxy/praxis/issues/354)) | Re-entrant chain support is still being developed in Praxis — building on it today introduces coupling to an evolving API |
| Single deployment unit (one Praxis binary) | Harder to run standalone without Praxis |

#### Single thin filter (our approach)

| Pros | Cons |
|------|------|
| Core logic is testable with `cargo test` and the existing mock harness | Praxis has less visibility into the orchestration pipeline (it sees one filter, not the internal steps) |
| The agentic loop is an explicit state machine — easy to reason about, debug, and extend | Requires a network hop in service mode (~1ms, negligible for LLM workloads) |
| Works standalone (axum) or behind any gateway — not coupled to Praxis's filter API | Two deployment units in production (Praxis + agentic-api service) unless using in-process mode |
| Adapter is thin (~50 lines), cheap to update if Praxis's filter API evolves | |
| Praxis can still use `agentic-core` as an in-process library via the adapter filter — no network hop needed | |

We choose the single-filter model because it keeps the orchestration core framework-agnostic, testable, and portable, while still offering Praxis native in-process integration via the adapter crate (D4, D6). As Praxis's re-entrant chain support matures, the integration model can be revisited.

### Why three layers

- **Testability.** Core logic is tested without any HTTP server or gateway infrastructure.
- **Portability.** Adding a new gateway adapter means writing one thin crate, not porting the entire system.
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
- **Core API design.** The `agentic-core` public API (`execute()`, `ResponsesRequest`, `ResponseStream`, `ExecutionContext`) needs careful design — it's the contract that all adapters depend on.
- **Praxis co-development.** We contribute `agentic-praxis` and work with the Praxis team to validate the backend routing model. This is simpler for both sides than re-entrant filter chains.
- **State services.** Response store (ADR-02), conversation manager, and tool registry are implemented natively in Rust within `agentic-core`. No external Python services in the request path.

---

## Open Questions

1. **Praxis filter API stability.** The `HttpFilter` trait and `HttpFilterContext` API are young. How stable is the contract we build the adapter against? Mitigation: the adapter is thin (~50 lines), so API changes are cheap to absorb.

2. **Built-in tool implementation.** `web_search`, `file_search`, `code_interpreter` are listed as Rust-native. These are non-trivial to implement. What's the MVP subset? Likely: MCP client first (delegates to external tool servers), built-in tools later.

3. **Guardrails integration point.** Input guardrails can run in Praxis (pre-routing) or in agentic-api (post-hydration, with full conversation context). Output guardrails must run in agentic-api (per loop iteration). The split needs to be validated with the guardrails team.

4. **In-process vs service mode trade-offs.** Mode 2 (service) adds ~1ms per loop iteration but gives process isolation and independent scaling. Mode 3 (in-process) eliminates the hop but shares failure domains. Which is the default recommendation for production?
