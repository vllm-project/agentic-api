# Design: gateway-owned tool classification for `/v1/messages` (Claude Code end-to-end)

> **Status:** proposal / follow-up to PR #131 (the Messages-native gateway tool loop, #115).
> **Depends on:** #131 merged.
> **Relates to:** #113 (Claude Code integration), the Responses-side ownership model in
> [`codex-integration.md`](./codex-integration.md), and the loop-consolidation note.

---

## Problem

PR #131 makes `POST /v1/messages` run the gateway tool loop when a request declares a
gateway-owned tool. It classifies gateway-owned **by name**:

```rust
fn is_gateway_owned_tool_name(name: &str) -> bool { name == "web_search" }
```

That is enough for a client that declares the lowercase `web_search` tool (curl, SDKs, our
own harness), but it does **not** make Claude Code's web search run through the gateway, and
the reason is instructive.

### What Claude Code actually sends (live-captured)

A real Claude Code request to `/v1/messages` declares ~27 **client-owned function tools**.
Its web search is one of them:

```json
{ "name": "WebSearch",
  "description": "Search the web. Returns result blocks with titles and URLs. US-only. ...",
  "input_schema": { "type": "object", "properties": { "query": {"type":"string"},
                    "allowed_domains": {...}, "blocked_domains": {...} } } }
```

Two facts follow:

1. **The name is `WebSearch` (PascalCase), a plain function** — not `web_search`, and not a
   typed Anthropic server tool. Our `name == "web_search"` predicate does not match it, so the
   request is proxied and the loop never engages. (Verified live: every default Claude Code turn
   routes `route="proxy"`.)
2. **It is semantically Claude Code's own tool.** Claude Code declares `WebSearch` expecting
   *it* executes the search (its own `allowed_domains`/`blocked_domains` handling, its
   "Sources:" formatting) and receives the `tool_use` back to run client-side.

### Why a *hardcoded* name alias is wrong — and why a *configured* one is right

There are two versions of "alias `WebSearch` to `web_search`", and only one is acceptable.

**Rejected — a hardcoded in-code alias** (`is_gateway_owned_tool_name` returning true for
`WebSearch` unconditionally). This:

- **Hijacks a client-owned tool by default.** Silently converting Claude Code's `WebSearch` to
  gateway-owned for *every* deployment means Claude Code never gets the `tool_use` it expects to
  run. That contradicts the ownership doctrine:
  > "`function` → client-owned by default ... unless configured as gateway-owned"
  > — [`codex-integration.md`](./codex-integration.md)
- **Is a silent heuristic.** The same doc bans detecting compatibility "by user agent, route, or
  heuristics." A hardcoded PascalCase match is exactly that.

**Shipped — an operator-configured alias** (`MESSAGES_GATEWAY_TOOL_ALIASES=WebSearch=web_search`,
empty by default). This is the *"unless **configured** as gateway-owned"* clause itself: nothing
is captured unless the operator opts in, so it is a deliberate configuration, not a silent
heuristic. It does not violate the doctrine — it *implements* the doctrine's escape hatch.

**Why not structural typed-tool detection for Claude Code?** Because Claude Code never sends a
typed server tool. Its web search on the wire is `{"name":"WebSearch", "input_schema": …}` — a
plain `function`, no `type` field (verified live). Structural `{"type":"web_search_20250305"}`
detection (below) is the right path for Anthropic-SDK clients that *do* send a typed tool, but it
is inert for the real Claude Code CLI/SDK. For those, a **name→executor mapping is the only
mechanism that can work** — the question is only whether it's hardcoded (no) or operator-configured
(yes). The two mechanisms are complementary, not alternatives.

## How Codex already gets this right (live-verified)

Codex talks the Responses API (`/v1/responses`), and it declares web search **structurally**:

```json
{ "type": "web_search", "name": null }
```

alongside its client tools (`{"type":"function","name":"exec_command"}`, …) and a
`{"type":"namespace"}` group. The Responses side classifies gateway-owned **by type**, and the
enum already aliases the known server-tool type tags:

```rust
#[serde(rename = "web_search_preview",
        alias = "web_search", alias = "web_search_preview_2025_03_11",
        alias = "web_search_2025_08_26")]
WebSearch(WebSearchToolParam),
```

Replaying a real Codex request to the gateway returns output items
`reasoning → web_search_call → reasoning → message` — i.e. the gateway executed the search
server-side and continued the loop, while Codex's `function`/`namespace` tools stayed
client-owned. **No name matching, no hijack, works today.**

The asymmetry is the root issue: Messages classifies by **name** (fragile), Responses by
**type** (robust). This is the DIR-1 item in the loop-consolidation note.

## Shipped (this change)

### Operator-configured client-tool → executor aliases

A `GatewayToolMap` on `ExecutionContext`, loaded from `MESSAGES_GATEWAY_TOOL_ALIASES`
(e.g. `WebSearch=web_search`), **empty by default**. When configured, the aliased client tool is
classified gateway-owned at every site (routing, registry build, both loops), dispatch
canonicalises the call name to the executor key, and `adapt_web_search_input` rewrites the
client's argument schema (`allowed_domains → include_domains`, `blocked_domains →
exclude_domains`) so Claude Code's `WebSearch` reaches the `web_search` executor. The raw request
still forwards to vLLM verbatim, so the model keeps the client's own tool contract; only the
executed call is adapted and hidden.

This is the doctrine's "unless configured" clause, and — per the section above — the only
mechanism that works for Claude Code, whose search is a client `function`, not a typed tool.

## Future (additive, not shipped here)

These extend the *same* `GatewayToolMap` classification seam; they do not replace the config
alias, they complement it for clients that speak a different shape.

### 1. Structural typed-tool detection

Recognise a **typed Anthropic server tool** (`{"type":"web_search_20250305"}` and versioned tags,
carried in `ToolParam.type_`) as gateway-owned automatically — for Anthropic-SDK clients that
send a typed tool. Inert for the current Claude Code CLI/SDK (they send `{"name":"WebSearch"}`),
which is why the config alias ships first.

### 2. Per-request opt-in override

For a per-request (rather than process-global) signal — `metadata.gateway_tools: ["WebSearch"]`
on the body (a header like `x-gateway-tools` is unreachable from the Claude Code SDK, which
exposes no custom-header knob — verified). Lets two clients with different tool casing coexist,
which the env-global map cannot.

### 3. MCP generalisation

MCP-over-Messages lands on the **same** seam: an `mcp_servers` request param is gateway-owned by
construction (the server connects out and runs the tool). A new *shape* to recognise, not a new
name to special-case.

## Non-goals

- Detecting "is this Claude Code?" by user agent or headers — banned by the documented doctrine.
  (The config alias keys on the declared tool name the operator opted in, not on client identity.)
- Reshaping `RequestPayload` / the Responses loop — that is the ADR-gated consolidation, not this.
- A gateway-side Claude Code runtime.
- Working around the executor's `include_domains` + `exclude_domains` mutual exclusion — Claude
  Code's `WebSearch` allows both `allowed_domains` and `blocked_domains`; a call setting both
  yields a graceful error `tool_result`. The constraint stays in one place (the executor).

## Validation done

- **Claude Code, live (G6e):** captured its real request; confirmed `WebSearch` is a client
  function (PascalCase), default sessions route `proxy`. A typed/lowercase `web_search`
  declaration *does* route `messages_loop` and execute against You.com with the call hidden —
  proving the loop is correct; only classification of Claude Code's own tool is the gap.
- **Codex, live (G6e):** captured its real request; confirmed `{"type":"web_search"}` is
  recognised structurally, routes through the loop, returns a `web_search_call` item.

Evidence: `contrib-track/agentic-api/stress/pr131_evidence/` (retained internally).
