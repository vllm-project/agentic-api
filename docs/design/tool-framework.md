# Design: Tool Framework

> Status: Accepted — implemented and shipping.
> References: [ADR-01 D7](../adr/ADR-01_core.md), [ADR-03 D3](../adr/ADR-03_gateway_integration.md)

> **As-built note.** This document began as a proposal and now reflects what
> shipped. The framework landed across several PRs and diverged from the
> original sketch in a few deliberate ways — most notably the `ToolHandler`
> trait split, a two-layer dispatch model, a trimmed `LoopDecision`, and a new
> `CodexNamespace` tool type. Those changes are called out inline and mapped to
> their PRs in [Implementation Status](#implementation-status). Sections that
> capture rationale (Principles, Alternatives Considered, Design Decisions) are
> preserved as-designed; the type/trait definitions below match the shipped code.

---

## Problem

Clients send heterogeneous tool types (`function`, `namespace`, `mcp`, `web_search`, `file_search`, `code_interpreter`). vLLM only speaks function calling — it produces `function_call` output items regardless of tool origin. The gateway must bridge both directions: normalize inbound tools for inference, and route outbound calls to their correct executors.

Today `ResponsesTool = FunctionTool`. This design replaces that with a type-aware framework that handles the full tool lifecycle for any tool type through a single pipeline.

---

## Principles

1. **One pipeline, many types.** The tool lifecycle is the same for all types. What varies is the behavior at each stage.
2. **vLLM is function-only.** Every tool type normalizes to `type: "function"` before inference. Permanent constraint.
3. **Routing by registry, not heuristics.** After inference, `function_call` items are looked up in a request-scoped registry that maps names back to origin type and config.
4. **Ownership decides execution.** Each `ToolType` is gateway-owned or client-owned (`ToolType::is_gateway_owned()`). Client-owned types (`function`, `codex namespace`) are never gateway-executed — the response returns `status: "requires_action"` and the client resolves them. Gateway-owned types (`web_search`, `mcp`, `file_search`, `code_interpreter`) are executed server-side *when a handler is registered*; today only `web_search` ships a handler (see [Implementation Status](#implementation-status)). A gateway-owned type with no handler is preserved, not executed.
5. **Additive.** New tool types implement a trait and register. The executor loop doesn't change.

---

## Architecture

```mermaid
graph TD
    subgraph "Request Phase (once per request)"
        REQ["Client Request<br>tools: mixed types"]
        PARSE["Parse + Validate<br>per-type schemas"]
        DISC["Discover<br>MCP: tools/list"]
        NORM["Normalize<br>all → type: function"]
        REG["Build Registry<br>name → type + config"]
    end

    subgraph "Inference"
        VLLM["vLLM<br>sees only function tools"]
    end

    subgraph "Execution Phase (per iteration)"
        ROUTE["Route — classify_round<br>registry lookup per call"]
        EXEC_GW["Gateway Execute — registry.dispatch<br>mcp / web / file / code"]
        PASS["Passthrough → requires_action<br>function / codex namespace"]
        LOOP["Inject Results<br>re-enter inference"]
    end

    REQ --> PARSE --> DISC --> NORM --> REG
    REG --> VLLM
    VLLM --> ROUTE
    ROUTE -->|gateway-owned| EXEC_GW
    ROUTE -->|client-owned| PASS
    EXEC_GW --> LOOP --> VLLM

    style REQ fill:#1a5c2a,color:#e0e0e0
    style VLLM fill:#1a5c2a,color:#e0e0e0
    style PARSE fill:#2a4a8a,color:#e0e0e0
    style DISC fill:#2a4a8a,color:#e0e0e0
    style NORM fill:#2a4a8a,color:#e0e0e0
    style REG fill:#2a4a8a,color:#e0e0e0
    style ROUTE fill:#2a4a8a,color:#e0e0e0
    style EXEC_GW fill:#2a4a8a,color:#e0e0e0
    style PASS fill:#2a4a8a,color:#e0e0e0
    style LOOP fill:#2a4a8a,color:#e0e0e0
```

---

## Pipeline Stages

Every request with tools passes through 7 stages. Stages 1–4 run once at request start. Stages 5–7 repeat per inference iteration.

| # | Stage | Generic (framework) | Type-Specific (handler) |
|---|-------|---------------------|-------------------------|
| 1 | **Parse** | Deserialize `tools[]`, classify by `type` | Validate required fields per type |
| 2 | **Discover** | Iterate handlers, collect discovered tools | MCP: `tools/list`. Others: no-op |
| 3 | **Normalize** | Flatten all into `Vec<FunctionTool>` for vLLM | MCP: schema → parameters. WebSearch: synthetic def |
| 4 | **Register** | Build `HashMap<name, ToolEntry>` | Each handler declares ownership of its tool names |
| 5 | **Route** | Lookup `function_call.name` in registry | Determine: gateway-execute or client-passthrough |
| 6 | **Execute** | Parallel execution with timeout + error isolation | MCP: JSON-RPC. WebSearch: HTTP API. Function: skip |
| 7 | **Emit** | Forward type-specific SSE events to client | MCP: 7 events. WebSearch: 2 events. Function: 0 |

Stages 1–4 produce two artifacts:
- **Normalized tools** — `Vec<FunctionTool>` forwarded to vLLM
- **Tool registry** — `ToolRegistry` consumed by dispatch for routing

---

## Core Types

### Tool Classification

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolType {
    Function,
    CodexNamespace,   // added by codex integration (#84): a namespaced group of
                      // client-owned function tools (e.g. `mcp__shell.run`).
    Mcp,
    WebSearch,        // internal routing discriminant; serializes as "web_search"
                      // while the wire tag is "web_search_preview".
    FileSearch,
    CodeInterpreter,
}

impl ToolType {
    /// Gateway-owned types are executed server-side; everything else
    /// (`Function`, `CodexNamespace`) is client-owned and handed back.
    pub const fn is_gateway_owned(self) -> bool { /* ... */ }
}
```

> **Drift from proposal:** `CodexNamespace` did not exist in the original
> sketch. Codex declares tools grouped under a namespace whose members are
> client-owned; they flatten to model-visible names for inference and restore
> to `{namespace, name}` on the way out. `is_gateway_owned()` is the single
> predicate the dispatch layer uses to split gateway vs. client calls.

### Request-Side Tool Param

Replaces `pub type ResponsesTool = FunctionTool`:

```rust
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesTool {
    #[serde(rename = "function")]
    Function(FunctionToolParam),

    #[serde(rename = "mcp")]
    Mcp(McpToolParam),

    #[serde(
        rename = "web_search_preview",
        alias = "web_search",
        alias = "web_search_preview_2025_03_11",
        alias = "web_search_2025_08_26"
    )]
    WebSearch(WebSearchToolParam),

    #[serde(rename = "file_search")]
    FileSearch(FileSearchToolParam),

    #[serde(rename = "code_interpreter")]
    CodeInterpreter(CodeInterpreterToolParam),

    // Codex namespace group of client-owned function tools (#84).
    #[serde(rename = "namespace")]
    Namespace(CodexNamespaceToolParam),

    // Forward-compat catch-all: unrecognized `type` deserializes here rather
    // than erroring, so a new upstream tool type is preserved, not rejected.
    #[serde(rename = "unknown", other)]
    Unknown,
}
```

`#[serde(tag = "type")]` makes this wire-compatible with existing
`{"type":"function",...}` requests. `#[non_exhaustive]` + the `Unknown` catch-all
means an unrecognized tool type is preserved rather than failing the request
(consistent with the roadmap's "unknown shapes are preserved, never executed").
The `web_search` aliases accept the dated OpenAI variants.

### Tool Registry

```rust
pub struct ToolEntry {
    pub tool_type: ToolType,
    pub config: Value,                            // serialised server-level tool param
    pub server_label: Option<String>,             // MCP: which server this tool belongs to
    pub handler: Option<Arc<dyn GatewayExecutor>>, // the executor for gateway-owned tools
}

pub struct ToolRegistry {
    entries: HashMap<String, ToolEntry>,
}

impl ToolRegistry {
    pub fn lookup(&self, tool_name: &str) -> Option<&ToolEntry>;
    pub fn gateway_owned<'a>(&self, calls: &'a [FunctionToolCall]) -> Vec<&'a FunctionToolCall>;
    pub fn client_owned<'a>(&self, calls: &'a [FunctionToolCall]) -> Vec<&'a FunctionToolCall>;

    /// Per-call dispatch: resolve the handler for one call and execute it.
    /// Returns `None` when the tool has no registered handler.
    pub async fn dispatch(&self, call: &FunctionToolCall) -> Option<GatewayDispatchResult>;
}
```

> **Drift from proposal:** the executor now lives on the `ToolEntry` as
> `handler: Option<Arc<dyn GatewayExecutor>>`, and per-call routing is a method
> on the registry — `dispatch()` — rather than free-standing `dispatch_tools`
> logic. The registry owns "resolve one call to its executor and run it"; the
> multi-turn loop (below) owns "how many rounds." See
> [Dispatch: two layers](#dispatch-two-layers).

### Loop Decision

```rust
#[derive(Debug)]
#[non_exhaustive]
pub enum LoopDecision {
    /// Gateway tools were dispatched this round; loop again with their outputs
    /// appended to the conversation.
    Continue,

    /// No gateway work remains — the turn is final and the loop terminates.
    Done,

    /// One or more calls are client-owned (plain `function` or Codex
    /// `namespace` tools); hand the turn back to the caller to execute.
    RequiresClientAction,

    /// The round cap was hit while the model was still requesting tools. The
    /// response is returned with `status: "incomplete"` rather than as an error.
    Incomplete(String),
}

fn classify_round(
    has_client_owned_calls: bool,
    gateway_results: &[GatewayCallResult],
    round: usize,
    max_rounds: usize,
) -> LoopDecision;
```

> **Drift from proposal:** the shipped enum has **four** variants, not five.
> `ContinuePartial` and a payload-carrying `RequiresAction(Vec<..>)` were
> dropped. The mixed gateway+client turn (the case `ContinuePartial` existed
> for) is handled by **precedence in `classify_round`**, not a dedicated
> variant: client-owned calls take priority, so a turn with both executes its
> gateway calls, records their outputs, and still returns `RequiresClientAction`
> in a single round — the client gets the resolved gateway result and the
> pending client call together. The variants are unit (no payloads); accumulated
> output lives on the payload, not the decision. `RequiresAction` was renamed
> `RequiresClientAction` to name *who* acts.

### Dispatch: two layers

The original sketch had a single `dispatch_tools`. As built, dispatch is two
composable layers with a clean seam:

```mermaid
graph LR
    subgraph L2["Layer 2 — multi-turn orchestration (executor/gateway.rs, #83)"]
        CR["classify_round → LoopDecision"]
        LOOP["run_until_gateway_tools_complete<br/>loops until Done / RequiresClientAction / Incomplete"]
    end
    subgraph L1["Layer 1 — per-call dispatch (tool/registry.rs, #82)"]
        DISP["ToolRegistry::dispatch(call)<br/>resolve handler → execute → GatewayDispatchResult"]
    end
    LOOP --> CR
    LOOP -->|"for each gateway-owned call this round"| DISP

    style L2 fill:#2a4a8a,color:#e0e0e0
    style L1 fill:#1a5c2a,color:#e0e0e0
    style CR fill:#2a4a8a,color:#e0e0e0
    style LOOP fill:#2a4a8a,color:#e0e0e0
    style DISP fill:#1a5c2a,color:#e0e0e0
```

- **Layer 1 — per-call (`ToolRegistry::dispatch`, #82):** resolves one
  `function_call` to its `handler` and runs it. Knows nothing about rounds.
- **Layer 2 — multi-turn (`classify_round` + the loop, #83):** decides whether
  the turn continues, is done, hands back to the client, or exhausts the round
  budget. Calls Layer 1 for each gateway-owned call, then re-infers.

This split is what lets Codex's client-owned path and gateway execution share
one loop vocabulary instead of forking. It is the subject of a proposed
layering ADR (see [Future Work](#future-work)).

---

## The ToolHandler / GatewayExecutor Traits

The proposal had one fat `ToolHandler` trait carrying `execute()`. As built the
trait is **split in two**, because `execute()` only applies to gateway-owned
tools — a `function` or `codex namespace` handler has no server-side execution,
so putting `execute()` on the shared trait would be a lie for those types.

```rust
// Every tool type implements this — parse/validate/normalize only.
pub trait ToolHandler: Send + Sync {
    fn tool_type(&self) -> ToolType;
    fn validate(&self, param: &Value) -> Result<(), ToolError>;
    fn normalize(&self, param: &Value) -> Vec<FunctionTool>;
}

// Only gateway-executed tool types implement this — it *requires* ToolHandler.
// Handlers are stored as `Arc<dyn GatewayExecutor>`, so the async method is
// written as `Pin<Box<dyn Future>>` (dyn-compatible) rather than `async fn`.
pub trait GatewayExecutor: ToolHandler + 'static {
    fn execute(
        &self,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        config: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>>;
}
```

Adding a gateway tool type = implement both traits + register. A client-owned
type (like `CodexNamespace`) implements only `ToolHandler`. No changes to the
executor loop, accumulator, or streaming path.

> **Drift from proposal:** (1) trait split `ToolHandler` / `GatewayExecutor`;
> (2) `Pin<Box<dyn Future>>` instead of `#[async_trait]`, for `dyn`
> compatibility behind `Arc`; (3) `discover()` and the `event_prefix()` /
> `output_item_type()` convenience hooks did not ship on the trait — SSE
> emission is handled in the gateway layer keyed on `tool_type`, and MCP
> discovery lives in the MCP handler rather than a generic trait method.

---

## Per-Type Behavior

| Stage | `function` | `codex namespace` | `mcp` | `web_search` | `file_search` | `code_interpreter` |
|-------|-----------|-------------------|-------|-------------|--------------|-------------------|
| Validate | name required | member names required | server_url required | (none) | vector_store_ids required | (none) |
| Discover | no-op | no-op | `tools/list` on server | no-op | no-op | no-op |
| Normalize | passthrough | flatten members → `FunctionTool` (`ns__member`) | McpToolDef → FunctionTool | synthetic `web_search(query)` | synthetic `file_search(query)` | synthetic `code_interpreter(code)` |
| Route | → client | → client (restore `{ns, name}`) | → gateway | → gateway | → gateway | → gateway |
| Execute | N/A | N/A | JSON-RPC `tools/call` | HTTP search API | vector store query | sandboxed container |
| SSE events | `function_call_arguments.*` | `function_call_arguments.*` | `mcp_call.*` | `web_search_call.*` (2) | `file_search_call.*` | `code_interpreter_call.*` |
| Response status | `requires_action` | `requires_action` | `completed` | `completed` | `completed` | `completed` |

`codex namespace` and `web_search` are the two ends actually shipping today
(`#84` and `#85`); `mcp` is in review (`#89`); `file_search` / `code_interpreter`
are declared `ToolType`s without handlers yet.

---

## Mixed-Tool Request Walkthrough

Request:
```json
{
  "tools": [
    {"type": "function", "name": "run_shell", "parameters": {...}},
    {"type": "mcp", "server_label": "db", "server_url": "http://db-mcp:8080"},
    {"type": "web_search_preview"}
  ],
  "input": "Find papers on RLHF, check our DB, then run the import script"
}
```

**Preparation:**
- Discover: MCP server returns `[query_papers, insert_paper]`
- Registry: `run_shell → Function`, `query_papers → Mcp`, `insert_paper → Mcp`, `web_search → WebSearch`
- vLLM sees 4 function tools

**Iteration 1:** Model calls `web_search("RLHF papers")` → gateway executes → loop back

**Iteration 2:** Model calls `query_papers("topic=RLHF")` → gateway executes via JSON-RPC → loop back

**Iteration 3:** Model calls `run_shell("python import.py")` → registry lookup → `Function` → **client-owned** → response returns `status: "requires_action"`

Client executes locally, submits `function_call_output`, inference continues.

> Note: a mixed turn (a gateway *and* a client call in the same model output)
> does not need an extra iteration. The gateway call executes and its output is
> recorded, and because a client-owned call is present the turn still returns
> `requires_action` in that same round — see the `classify_round` precedence
> under [Loop Decision](#loop-decision).

---

## Implementation Status

The proposal's PR plan (A–E) shipped, reorganized around the merged registry
and the two-layer dispatch model. Actual PRs:

| Area | PR(s) | Status |
|------|-------|--------|
| Tool types + registry + `ToolHandler` trait + `FunctionHandler` + normalize | **#80** | ✅ merged |
| Handler-in-`ToolEntry` + per-call `ToolRegistry::dispatch()` (MCP gateway design) | **#82** | ✅ merged |
| `web_search` gateway tool (first `GatewayExecutor`) | **#85** | ✅ merged |
| Codex integration → `CodexNamespace` client-owned type + flatten/restore | **#84** | ✅ merged |
| Codex namespace invariant tightening (collision reject) | **#91** | ✅ merged |
| Multi-turn loop: `classify_round` + `LoopDecision` (Layer 2) | **#83** | 🔄 in review |
| Remote MCP gateway (`read_resource`, `tools/call`) | **#89** | 🔄 in review |
| `file_search`, `code_interpreter` handlers | — | declared `ToolType`, no handler yet |

The trait split, `Pin<Box>` async, `CodexNamespace`, the two-layer dispatch, and
the four-variant `LoopDecision` are the substantive divergences from this doc's
original sketch — each is annotated inline above.

## Future Work

- **Layering ADR.** Promote the two-layer dispatch (per-call `registry.dispatch`
  + multi-turn `LoopDecision`) from an implementation detail to a recorded
  decision, so later APIs (Messages, Interactions) reuse the same loop instead
  of forking. Gated on #83 landing so the ADR describes shipped code.
- **`GatewayAccumulator` (streaming).** Today the "hide gateway-owned calls,
  emit the synthetic public frame" logic exists twice — once for blocking
  (`public_output_items`) and once for streaming (`emit_gateway_*_events`). A
  `GatewayAccumulator` stage (Raw → Gateway → Public, mirroring
  `ResponseAccumulator`) would classify once and let both paths consume it.
  Concrete once #89 lands the second gateway frame type.
- **Per-tool-type execution config.** `GATEWAY_TOOL_TIMEOUT` and the concurrency
  window are file-private consts today. When tool types with materially
  different latency profiles land (an MCP resource fetch vs. a web search), the
  existing `execute_gateway_call_with_timeout(timeout)` seam makes promoting
  them to per-type config additive.
- **`file_search` / `code_interpreter` handlers.** Both are declared `ToolType`s
  awaiting `GatewayExecutor` impls.

---

## Design Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Registry-based routing | Name prefixes leak implementation into the model's tool namespace. Registry is invisible to inference. |
| D2 | Request-scoped registry | Different requests may target different MCP servers. Global state would require sync and conflict resolution. |
| D3 | `function` never gateway-executed | Matches OpenAI spec. Enables agent clients (Codex, etc.) that own their tool implementations. "No client delegation" means the gateway doesn't punt *its* work — not that function tools can't exist. |
| D4 | Mixed turns resolved by `classify_round` precedence, not a `ContinuePartial` variant | The proposal added `ContinuePartial` for turns with both gateway and client calls. As built, `classify_round` gives client-owned calls precedence: the gateway calls still execute and their outputs are recorded, and the turn returns `RequiresClientAction` in one round. Fewer variants, no payloads on the decision, same behavior. |
| D5 | MCP transport | The proposal called for a stateless client (fresh connection per request). #89 introduces a connection pool keyed on `server_url`; see that PR for the current stance. |
| D6 | `ResponsesTool` uses `#[serde(tag = "type")]` | Wire-compatible with existing `{"type":"function",...}` — no client migration needed. |
| D7 | `ToolHandler` split into `ToolHandler` + `GatewayExecutor` | `execute()` only applies to gateway-owned types; keeping it on the shared trait would force `function`/`codex namespace` handlers to implement a method they can never honor. The `GatewayExecutor: ToolHandler` supertrait keeps the contract honest and is `dyn`-stored as `Arc<dyn GatewayExecutor>`. |

---

## Alternatives Considered for `function` Tool Handling

Decision D3 (`function` is never gateway-executed, returns `requires_action`) is the most debatable choice. Here are the alternatives we evaluated:

| # | Alternative | Behavior | Why rejected |
|---|-------------|----------|--------------|
| A | **Reject function tools entirely** | Validate at parse time — if `type: "function"` is present, return 400. Force clients to back all tools with MCP servers. | Breaks OpenAI spec compatibility. Prevents agent clients (Codex, Claude Code) from using their natural pattern. Unnecessarily opinionated. |
| B | **Ignore + warn** | Accept `function` tools, normalize to vLLM, but if model calls one: drop the call silently, log a warning, and continue inference without it. | Silent data loss. Model asked for a tool result and gets nothing — produces hallucinated or degraded responses. Violates least-surprise. |
| C | **Search MCP servers for matching name** | When model calls a `function` tool, check if any registered MCP server happens to expose a tool with that name. If found, execute via MCP. If not, fall back to `requires_action`. | Spooky action at a distance. Client declares `type: "function"` expecting to own execution, but gateway silently intercepts it if an MCP server has a name collision. Also adds latency (extra `tools/list` queries). |
| D | **Gateway-execute all (require registered executor)** | Every `function` tool must have a backing executor configured in gateway config. No `requires_action` at all. | Requires operators to pre-configure every tool. Impossible for dynamic agent clients that generate tool definitions at runtime. Breaks the most common agentic pattern. |
| E | **Configurable per-request** | Add a field like `function_execution: "client" \| "gateway"` to let the client choose. | Over-engineering for MVP. Adds complexity to every code path. If a real use case emerges, we can add it later without breaking the default. |

**Chosen: passthrough with `requires_action`** — matches OpenAI spec exactly, zero surprise for clients, and cleanly separates "tools the gateway owns" from "tools the client owns" based solely on the `type` field the client already provides.

---

## Open Questions

Several of these were resolved as the framework shipped; resolutions noted.

| # | Question | Resolution |
|---|----------|-----------|
| Q1 | What if a discovered/namespaced tool name collides with another declared tool? | **Partially resolved (#91):** a Codex-namespace member that would flatten onto an already-declared name is a hard `ToolError` at registry-build time (`resolve_namespace_members`). Plain duplicate `function` names (and duplicate namespace *members*) remain last-write-wins with a `warn!` log — not a hard error. Tightening the plain-duplicate case is open. |
| Q2 | How does a mixed gateway+client turn look to the streaming client? | **Resolved (#83):** gateway tool events stream in real time during the round; the turn then ends `requires_action` in that same round (client-owned precedence in `classify_round`). No `ContinuePartial`. |
| Q3 | Should `tool_choice: {function: {name: "x"}}` work for discovered/namespaced tools? | **Resolved (#84/#91):** yes. vLLM sees all normalized functions; a forced namespaced name resolves through the namespace map. `tool_choice` names are validated as non-empty. |
| Q4 | Should `prepare_tools` be a Praxis filter or part of `execute_loop`? | **As built:** part of the core loop (`run_until_gateway_tools_complete`), not a per-stage Praxis filter. Praxis wraps the whole loop (ADR-03). |
| Q5 | Should the two-layer dispatch model be recorded as an ADR? | **Open** — proposed, gated on #83. See [Future Work](#future-work). |
