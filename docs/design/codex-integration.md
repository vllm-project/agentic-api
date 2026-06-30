# Design: Codex CLI Integration

> **References:** [Issue #54](https://github.com/vllm-project/agentic-api/issues/54),
> [PR #67](https://github.com/vllm-project/agentic-api/pull/67)
> **Owner:** @haoshan98 for Codex compatibility. @ashwing PR #67 owns the generic tool framework.

---

## Summary

`agentic-api` should work as an upstream layer for Codex CLI while routing inference to vLLM-supported models.

This PR is an MVP compatibility slice. It lets `agentic-api` accept and preserve Codex-used Responses traffic now,
without waiting for the full generic tool framework from PR #67.

The important split:

- **This PR:** preserve Codex request/response shapes and continuation state.
- **PR #67:** formalize generic tool normalization, execution, registry, ownership, and loop decisions.

---

## Current PR Scope

This PR should do only the minimum needed for Codex compatibility:

- Add this standalone design doc.
- Accept Codex-used tool declarations without rejecting requests.
- Preserve unknown tool declarations and unknown input/output items as raw JSON.
- Preserve optional `namespace` on `function_call`.
- Preserve `tool_search_call` and `custom_tool_call` shapes.
- Preserve assistant tool-call items through `previous_response_id` rehydration.
- Add model alias routing for Codex-facing model names to local vLLM models.
- Add lightweight helper types/tests that document what #67 should formalize later.

This PR should **not** build a second generic tool framework.

---

## Deferred To PR #67

PR #67 should own the formal shared tool system:

- `ToolHandler` / `Tool` trait shape.
- Generic tool normalization before `call_inference()`.
- Request-scoped tool registry.
- Client-owned vs gateway-owned dispatch.
- Requires-action / client-action loop decision.
- Live `execution_loop` orchestration and streaming tool events.

The helper types in this PR are temporary. They express Codex requirements, but the canonical versions should come
from #67. After #67 lands, this slice should plug into or be refactored onto those abstractions.

---

## Compatibility Rules

The gateway should not detect requests by user agent, route, or "is this Codex?" heuristics. Compatibility is
driven by Responses tool shapes and execution semantics, so it can be always on.

| Shape | Behavior |
|-------|----------|
| `function` | Client-owned by default. Preserve declaration and return matching calls to the client unless configured as gateway-owned. |
| `namespace` | Model-facing grouping for function tools. Do not treat namespace as a separate executable call type. |
| `tool_search` | Client-owned only when `execution == "client"`. Hosted/non-client search is provider-owned. |
| `custom` | Client-owned by default. Preserve free-form / grammar metadata. |
| Unknown tool | Preserve as raw JSON. Never execute by default. |

For response items:

| Response item | Behavior |
|---------------|----------|
| `function_call` | Preserve optional `namespace`. |
| `tool_search_call` with `execution == "client"` | Return to the client for local deferred discovery. |
| Hosted / non-client `tool_search_call` | Do not execute locally. Leave to provider-specific handling. |
| `custom_tool_call` | Preserve free-form `input`; do not coerce into JSON function arguments. |
| Unknown output item | Preserve as raw JSON. Never execute by default. |

---

## Requirements For #67

The generic framework should preserve enough metadata for Codex-compatible behavior:

- raw original tool JSON
- model-visible tool name
- original client-visible identity
- optional namespace or an equivalent unambiguous key
- execution owner: `Client`, `Gateway`, or provider-owned
- raw hints such as `execution`, `format`, and `defer_loading`

If namespaced tools need disambiguation, a split identity is useful:

```rust
pub struct ToolName {
    pub namespace: Option<String>,
    pub name: String,
}
```

This avoids collisions such as two different namespaces both defining a tool named `run`.

---

## Continuation

Codex-owned tool calls must survive response-store continuation.

Expected rehydration shape:

```text
prior context + assistant tool call + Codex tool output + new input
```

On a turn that returns client-owned tool calls, storage should keep the assistant call item. On the next turn, Codex
submits the matching tool output item, and `previous_response_id` should rebuild the full sequence.

---

## Model Aliases

Model aliases route Codex-facing model names to local vLLM models:

```toml
[model_aliases]
codex-compatible = "qwen3-coder"
```

Alias resolution is only model routing. It must not imply approval, auto-review, or human-confirmation behavior.

---

## Test Plan

Current PR tests should cover:

- `function`, `namespace`, `tool_search`, `custom`, and unknown tools round-trip.
- Extra fields remain preserved.
- `function_call.namespace` round-trips.
- `tool_search_call` and `custom_tool_call` remain raw-compatible.
- Unknown input/output items remain raw JSON.
- `previous_response_id` rehydrates assistant tool calls before tool outputs.
- Model aliases resolve on executor and proxy paths.

Post-#67 tests should prove the same behavior through the formal tool framework.

---

## Open Questions

1. What exact requires-action payload type should #67 expose?
2. Should #67 use split `ToolName { namespace, name }` or a different unambiguous registry key?
3. Which Codex-used fields should become typed framework fields, and which should remain raw metadata?
