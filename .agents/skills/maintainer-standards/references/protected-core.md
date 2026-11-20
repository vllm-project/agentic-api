# Protected core

The core is the layering of the gateway: the request path every API shares, the two context objects that carry it,
and the shared types. Since the RFC [#500](https://github.com/vllm-project/agentic-api/issues/500) refactoring, these
paths are maintained as they are and protected from drifting or gaining helpers around them. New features extend the
core as described below; changing one of its contracts needs an RFC issue and maintainer agreement first, and the PR
names the extension point it used or links that RFC.

## Layers and boundaries first

Maintainability depends on every piece of code living in the layer and module that owns it. The layers are transport
handlers in `agentic-server`, then the executor (rehydration, engine execution, persistence), then the handlers in
`executor/modes/` and the services and stores beneath them, with `types/` shared by all; the dependency direction is in
`AGENTS.md` and `ARCHITECTURE.md`. Before adding anything, name the layer and the owner it belongs to. Reject code
that reaches past a layer (the executor touching a store, transport deciding output items, storage deciding turn
policy) or that lifts a lower module's data up into a shared object because that was easier to reach.

## One path from rehydration to persistence

Every transport (HTTP, SSE, WebSocket, split execution) and every state source (conversation, previous response,
session parent, stateless input, restored agent tree) runs the same path:

1. **Rehydration** (`executor/rehydrate.rs`: `rehydrate`, which begins a session turn's lease, over
   `rehydrate_with_continuation`) builds the turn from the request and its history, with one branch per state source.
   Input is validated at entry and once more on the final assembled input.
2. **Engine execution** (`engine.rs` and the stages in "Target streaming pipeline and ownership boundaries" in
   `ARCHITECTURE.md`) runs the inference rounds and the tool loop. The response accumulator (`executor/accumulator/`)
   folds one round into output; a new output-item kind enters through its typed slot and active-item state, never as
   a feature-specific field or branch on `ResponseAccumulator`.
3. **Persistence** (`executor/persist.rs`: `persist`, `persist_turn`, `commit`; the handlers' `persist` in
   `executor/modes/`; `ResponseCheckpoint`) stores the client-facing turn. See "What a turn persists" in
   `ARCHITECTURE.md`.

Extend the stage that owns the behavior. Do not add a second path, mode, or step for one feature, a `*_with_*` or
`*_for_*` variant beside an existing function, a validation flag, a rewrite of `enriched_request.input`, storage
functions that encode call, tree, or turn policy, or extra queries and transactions on every turn for one feature.

## Lean context objects

`ExecutionContext` holds process-wide owners; `RequestContext` holds one turn (`original_request`,
`enriched_request`, the checkpoint, IDs). Both sit on every call in the path, so what goes on them changes the whole
core. Assess every addition:

- **Is this the right layer?** `ExecutionContext` holds the owner of a layer: a handler in `executor/modes/`, the
  gateway executors, a service, configuration validated at load. Data, caches, per-feature flags, or a field that
  only one module reads belong in that module or the layer below it, reached through its owner.
- **Is it too heavy?** Per-request data, large payloads, and derived state do not belong on `ExecutionContext`.
  Anything put on `RequestContext` is carried through every round and agent, so keep it to the turn's own state.
- **Does it ripple?** A design that adds a field and then threads a new argument through handler, executor, engine,
  and lowering signatures is the wrong design. Put the data with the owner that uses it, or derive it where it is
  needed from what is already passed. A PR whose signature changes reach across layers is sent back.

## Shared types and conversions (`types/`)

`types/` owns every wire and domain shape the other areas pass around, the conversions between them, and validation
on the owning type (see the `types/` section of `ARCHITECTURE.md`).

- **Contract:** one type per concept and one conversion per pair of shapes.
- **Extend by:** a variant, field, rule, or conversion on the existing type, with its callers updated.
- **Do not:** keep a workaround beside the owner (a parallel type, a second validator, a conversion helper elsewhere)
  so the owner does not have to change. If it cannot be extended cleanly, refactor the owner. The
  [engineering](../../agentic-api-engineering/SKILL.md) skill lists how to find it.

## Two normalizations, model-facing only

Tool declarations become `function` tools at
`ToolDeclaration::to_function_tools`, with gateway results returned to the model as `function_call_output`; history
items become model input at `ResponsesInput::model_input` and `normalized_model_input`, which lower gateway calls,
custom and shell calls, reasoning, message files, and compactions, and drop gateway-only items such as
`mcp_list_tools`. `ARCHITECTURE.md` describes both ("Output-to-input conversion for continuation rounds" and the
`tool/` section). A new tool or item kind gets its model-facing form only there; history, rehydration, and
persistence keep the client-facing shapes.
