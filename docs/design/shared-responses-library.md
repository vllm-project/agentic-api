# Shared inference and session contracts

## Status and scope

This is the contract proposal for [#360](https://github.com/vllm-project/agentic-api/issues/360), under
[RFC #359](https://github.com/vllm-project/agentic-api/issues/359). It follows the
[core-owned interface and dependency-injection proposal](https://github.com/vllm-project/agentic-api/issues/361#issuecomment-6163200816).
Cross-project agreement is pending; the review record below lists the decisions still needed.

**Implemented in this branch:** `RoundIngestion::push_frame` accepts the existing `EventFrame`, also used by the HTTP
path after normalization. Both paths use `ResponseAccumulator` for lifecycle validation and output assembly, then
the existing tool translator. Client sequencing remains with `GatewayStreamAccumulator`. Reuse `EventPayload`,
`SSEEventType`, `ResponseUsage`, and the existing output-item conversions; no separate inference-event enum or
published Responses crate is needed.

The branch also implements `executor/inference/http/` and `executor/inference/direct/`, with direct runtime injection
through `ExecutionContext::with_direct_inference`. The unpublished `agentic-vllm` crate loads shared vLLM model
resources, prepares each model request, and converts native output into existing events. This starts implementation
work from #361/#362 alongside the #360 design. Its integration guide is in `crates/agentic-vllm/README.md`.

**Proposed below:** capability discovery, richer deadlines/cleanup contracts, session/lifecycle metadata, exact input
identity, and distributed compatibility. gRPC and session-aware routing are not implemented. A live engine smoke test and model-profile qualification remain required before claiming serving parity.

## Owners and existing extension points

| Owner | Responsibility and reuse |
| --- | --- |
| Core `types/` and `events/` | Reuse existing request, input, tool, usage, output, and event types and their conversions. Add contracts only for missing backend/session semantics. |
| Core executor | Resolve history, inject the chosen backend, run inference rounds and tools, compact, persist, and deliver client events. |
| `executor/inference/http/` | Existing HTTP encoding, transport, and SSE framing. |
| `executor/inference/direct/` | Injected direct boundary with attempt cancellation, timeouts, and event limits. |
| `executor/prepare.rs` and `upstream.rs` | Shared tool/request preparation and model-visible projection before each round, for both modes. |
| `events/` and `executor/accumulator/` | Normalize events and perform the single lifecycle/identity/assembly implementation. |
| `executor/pipeline/ingest.rs` | Own the shared `EventFrame` input beneath framing, preserving translation and finalization. |
| `executor/translate/` and `executor/relay/` | Restore public tool shapes and deliver ordered events using the existing bounded path. |
| Shared vLLM libraries | Model rendering, tokenization, multimodal preparation, decoding, reasoning/tool parsing, and engine communication. |
| llm-d | Endpoint selection, canonical cache-key mapping, residency indexing, and session policy. |
| vLLM EngineCore | Inference, KV-cache mechanisms, cache events, and supported cache-control actions. |

`agentic-server-native` now composes the shared gateway with a supervised headless engine. The CLI selects HTTP
with `--upstream` and direct inference without it. `model.config` provides startup model preparation; `config.toml`
continues to configure shared gateway services. See the [configuration guide](../guides/configuration.md#direct-inference-in-source-builds).

The implemented dependency direction is:

```text
agentic-server-native                 (unpublished composition binary)
    +-- agentic-server --> agentic-server-core
    +-- agentic-vllm   --> agentic-server-core
            +-- shared vLLM frontend libraries
```

`agentic-vllm` owns model recipes, the native runtime builder, preparation/event adapters, and frontend shutdown.
The composition binary supervises the engine; the shared gateway serves its catalog and readiness without HTTP probes. Core's interface must not expose
`ChatLlm`, engine endpoints, protobuf messages, or HTTP clients. The application constructs and injects the backend;
published core and the existing server do not depend on the native integration package.

The existing `agentic-llm-d` crate provides hydrate/persist services to an external orchestrator. It is not the
outbound gRPC inference adapter. The existing Messages loop is also a distinct integration path: this initial
Responses event profile does not establish Messages parity or authorize replacing its protocol handling.

This resolves the ownership question raised by [#18](https://github.com/vllm-project/agentic-api/issues/18): Agentic
API supplies resolved history and lifecycle intent; llm-d owns fleet cache policy. Do not add a second cache index,
renderer, canonical hash implementation, or cache offloading service to the gateway.

## Implemented request and preparation boundary

`DirectInferenceBackend::generate(UpstreamRequest, GenerationControl)` is object-safe and returns
`ExecutorResult<EventStream>`. `EventStream` yields `ExecutorResult<EventFrame>`; `GenerationControl` holds the unique
attempt ID, public response ID, cancellation token, and event-size limit. Existing `ExecutorError`, `ResponseUsage`,
and engine usage aggregation are reused. `DirectInference` adds only a source-preserving runtime failure to the
executor error enum. `TryFrom<WireEvent>` uses existing normalization and `serde_json::Error`; it adds no new error enum.

Preparation has two levels. `VllmBackend::connect` loads tokenizer, renderer, and parser resources once and connects
the engine client. Shared `executor/prepare.rs` resolves request tools before orchestration; each round uses the
existing model-visible projection, then the native frontend renders/tokenizes that request. HTTP delegates model
preparation to its upstream. History resolution, persistence, response budgets, sessions, tool orchestration, usage
aggregation, and compaction use the same executor path in both modes.

Direct admission and each event poll honor the configured streaming timeout; dropping admission or the stream
cancels the attempt. The frontend owns engine abort and runtime tasks. The executor adds no buffering channel and
awaits client delivery before polling another event. However, the pinned vLLM engine client uses an internal unbounded
output queue. Event-size and retained-response limits do **not** bound that queue; an upstream queue budget and live
slow-consumer/cancellation qualification are still required for an end-to-end memory bound.

## Proposed distributed request and control extensions

The broader backend contract extends the implemented direct boundary with session metadata, capability discovery,
and cleanup registration. `capabilities()` would report the selected backend/model profile. The names in this section
describe future requirements; reuse existing request, event, usage, and executor-error types when extending them.

| Contract | Required content and invariant |
| --- | --- |
| `InferenceRequest` | A unique inference-attempt ID, selected model, resolved model-visible input, normalized function tools and tool choice, generation settings, optional session coordinates, and explicitly supported extensions. |
| Model-visible input | Ordered messages, reasoning, calls and call outputs after history resolution and current compaction. No unresolved `previous_response_id`, storage handle, or gateway-only projection is sent to the model. |
| Generation settings | Preserve presence and supported values for sampling, output limits, stop behavior, reasoning, structured output, and parallel calls. An omitted value is not automatically the same as zero or false. |
| `InferenceControl` | Cancellation, an optional monotonic deadline, and limits for input/preparation, queued events, individual events, retained output, and cleanup. |
| `Generation` | One ordered stream of `Result<EventFrame, InferenceError>`, cancellation on unfinished drop, and runtime-owned cleanup that can be awaited during shutdown. |
| `InferenceError` | Matchable unsupported-capability, invalid-input, cancellation, deadline, resource-limit, protocol, and backend-failure categories, preserving underlying sources for diagnostics. |

Extend the existing `RequestPayload::to_upstream_request` and its owning types when an owned request is needed.
That is already the lowering point for `ResponsesInput::model_input`, namespace/custom/shell forms, tool declaration
validation, and model-visible tool choice. An adapter must consume that projection, not implement a second history
filter or tool normalizer. Keep persistence on the existing client-facing history path.

The response ID belongs to orchestration and remains stable across a turn's inference rounds. An attempt ID belongs
to one submission and is used for cancellation and deduplication. Neither is a session ID or cache key. An inference
adapter must not replace the public response ID with an engine request ID.

Typed request fields are validated at the owning boundary for both Rust callers and deserialization. Known
extensions require a typed representation and an explicit capability decision; an untyped JSON map is not the
backend contract. Existing opaque HTTP pass-through behavior remains a separate compatibility path.

### Events, terminal outcomes, and usage

Use the existing `EventFrame` envelope, `SSEEventType` classification, and `EventPayload` variants. The SSE name
identifies Responses event semantics; it does not require an SSE transport. HTTP normalizes parsed upstream data into
that representation. A native adapter converts shared-vLLM output directly into the same representation. Neither path
needs another event enum or a native-to-SSE-text-to-event round trip.

```text
HTTP bytes -> SSE framing -> existing event normalization --+
                                                          +-> RoundIngestion::push_frame
shared-vLLM output -> native EventFrame conversion ---------+       -> ResponseAccumulator
                                                                  -> tool translation
                                                                  -> existing delivery / GatewayStreamAccumulator
```

`TryFrom<&EventPayload>` on the existing output types constructs items; `ApplyDone` and the accumulator's `MergeDone`
implementations reconcile completion with retained deltas. `ResponseAccumulator` owns lifecycle, identity, conflict
checks, and retained-byte accounting. `GatewayStreamAccumulator` owns public sequence numbers, output offsets, and
client lifecycle emission. Native conversion supplies events to these owners; it does not duplicate their rules.

The proposed native profile selects strict validation and follows this order:

1. `ResponseCreated`, then `ResponseInProgress`.
2. Zero or more output-item lifecycles: `OutputItemAdded`, optional content/argument events, then `OutputItemDone`.
   Item IDs, function call IDs/names, and output indexes remain stable. Interleaved items keep their own delta order.
3. Exactly one `ResponseCompleted`, `ResponseIncomplete`, or `ResponseFailed`, followed by EOF. A stream error
   terminates the attempt instead; EOF without a terminal event is a protocol error, not success.

Every active item must be complete before a strict terminal event, including incomplete and failed responses.
Completion supplies authoritative content; it does not authorize inventing a complete function call after an
interrupted parse. If the adapter cannot close an item truthfully, it returns a stream error and the executor uses
its existing failure path. It must not execute a partial tool call or publish a successful continuation checkpoint.
HTTP retains its configured strict/lenient policy; introducing a native adapter does not change compatibility rules.

`EventFrame` currently includes both an extracted payload and a `WireEvent` used by validation and delivery.
Adapters must preserve their consistency, including event kind, IDs, indexes, terminal details, and usage. Existing
compatibility fields such as `OutputItemDone.item` and `WireEvent.rest` still contain JSON values. When stronger native
construction guarantees are needed, extend the existing event model and conversions with the existing typed output
items and terminal payloads. Do not introduce a parallel inference-event model or a second output parser. HTTP-only
unknown fields must keep their existing compatibility behavior. Admission must reject native requests whose output
cannot be represented faithfully; silently discarding fields is not a conversion strategy.

Reuse `Option<ResponseUsage>` for round accounting. Reasoning is a subset of output, and cache reads are a subset of
input. The existing engine combines per-round counts once. Adapters must check nonnegative counts, subset relations,
and checked conversion/addition within the public signed integer range. Cache-read counts do not prove current
residency and do not imply cache-write or billing counters. Absent usage means unavailable, not zero. The existing
usage type defaults its required counters to zero for HTTP compatibility. Its optional `cache_write_tokens`
counter preserves absence; the native adapter fills it from the engine's reported cache-creation count.
Native usage capability claims must state which counters are known. Any availability extension belongs on the
existing usage contract, with an explicit public mapping, before advertising complete usage support. Do not add
another usage struct or manufacture cache-hit or reasoning counts.

| Shared vLLM chat output | Core adaptation |
| --- | --- |
| `Start` | Emit the ordered start/in-progress lifecycle, preserving caller-owned response identity. |
| `BlockStart`, `BlockDelta`, `BlockEnd` | Convert to existing item/content start, delta, and completion payloads with stable IDs and checked `u32` indexes. |
| `ToolCallStart`, `ToolCallArgumentsDelta`, `ToolCallEnd` | Preserve the call ID, name, argument order, and relation to its output item. |
| `Done` | Reconcile the final assistant message with existing items; map stop/length/abort explicitly and attach available usage once. |
| `LogprobsDelta` and additional metadata | Require a supported representation when requested; otherwise reject that request during admission. |
| Error or cancellation | End the attempt through the typed error/failure contract and clean up the engine request. |

Terminal-only generation (`ChatRequest::intermediate = false`) also needs item start/completion events reconstructed
from the authoritative final message. It must not duplicate items already emitted in incremental mode. A tool call
ends an inference round; orchestration decides whether to execute a built-in tool, await client-executed function
output, or finish the response.

### Capability reporting

Capabilities are the intersection of the backend implementation, loaded model/frontend, transport schema, and runtime
configuration. Discovery must identify their versions and limits. A parsed field or transport envelope is not proof
that the backend implements its semantics.

| Capability group | Reported support and admission behavior |
| --- | --- |
| Output | Text, reasoning text versus reasoning summary/opaque state, function calls, parallel calls, multipart output, logprobs, structured output, terminal-only output. Reject requested unsupported forms before submission. |
| Input/preparation | Model/tokenizer/template/parser revisions, supported media and limits, LoRA selection, cache isolation, rendering location, and prepared-token input. |
| Controls | Cancellation during preparation and execution, deadline propagation, event/retained-data limits, and known usage counters. |
| Session policy | Lifecycle notifications, soft affinity, retention actions/limits, and separately advertised worker-pinned sessions. |
| Locality | Exact-input production, canonical key version, subscribed cache groups/media, event replay/gap recovery, and engine/publisher identity. |

Required inference semantics must fail clearly if unsupported. Optional affinity or retention may degrade to ordinary
routing/inference only when that fallback was requested and its result is observable. Tenant isolation is never an
optional optimization. Backend selection must not silently drop the controls tracked in
[#314](https://github.com/vllm-project/agentic-api/issues/314) and
[#330](https://github.com/vllm-project/agentic-api/issues/330).

### Cancellation, deadlines, bounds, and retries

Cancellation applies during preparation, submission, generation, and blocked delivery. Closing the downstream stream
signals cancellation; the runtime owns aborting the submitted attempt and joining its tasks. Drop can signal but
cannot await cleanup. The generation guard therefore needs a supervised cleanup owner with a bounded shutdown wait
and reported cleanup errors. Cancellation must not leave an orphan producer blocked on a full channel.

The deadline includes preparation and queueing. Convert the remaining budget to a transport deadline at each hop;
do not send a process-local `Instant` over the network. Retries and tool rounds consume the enclosing turn's remaining
budget rather than resetting it. An expired deadline prevents another submission.

Keep synchronous ingestion inline. Prefer a pull-driven stream; every actual queue must declare both an entry limit
and a byte budget or maximum event size. A full queue applies awaited backpressure, subject to cancellation/deadline.
An oversized event fails with a resource-limit error. Bounds include parser buffers, media, authoritative completion
payloads, and terminal events, not only small deltas. Reuse `ExecutorResponseBudget` for retained output and the
existing delivery limits; a queue limit is not a total-memory limit. All spawned tasks need a cancellation and join
owner. Capacity values and cleanup timeouts must be explicit in each tested deployment profile.

There is no transparent retry after the first semantic event, including `ResponseCreated`. Before any event, retry only a
classified transient failure when non-acceptance or completed cancellation of the previous attempt is established,
and when the caller's attempt/deadline budget permits. An ambiguous submission result is not permission to start a
second generation. A retry has a fresh attempt ID; tool execution and persistence are never implicitly replayed.

## Frontend placement

| Deployment | Preparation and parsing | Execution |
| --- | --- | --- |
| Native, #362 | Shared vLLM chat/text request processors and output parsers in the Agentic API process, owned by `agentic-vllm`. | Shared engine client talks to headless Python EngineCore processes. |
| Distributed, #363 | Prefer the same frontend preparation before routing and the same parsers on returned tokens. Preserve prepared input and parser context across the transport adapter. | llm-d selects a worker after resolved input is available, then forwards through the agreed inference protocol. |
| Remote preparation, optional | A version-compatible vLLM rendering service can prepare input before placement. Carry its exact tokens, media, effective grammar, and reasoning-gate metadata into execution. Parsing placement and context must also be agreed. | The selected worker executes that prepared input without divergent rendering/truncation. |
| HTTP compatibility | The upstream owns its existing model preparation/parsing; core normalizes Responses events. | Existing compatible HTTP provider. Exact routing needs separate renderer-exact evidence. |

The pinned vLLM revision exposes `ChatRequestProcessor::prepare` and `Render.RenderChat`. The latter returns token IDs,
effective structured-output constraints, reasoning-gate settings, and prepared media. Its `omit_media_kwargs` mode
provides routing metadata only: that output cannot be submitted directly for generation. An execution request must
carry the required features and respect both client and server message limits. Reuse prepared data or validate exact
identity when re-preparing; otherwise the router scored a different request from the one the worker executes.

The selected composition must preserve instructions, tool-schema ordering, reasoning replay, template options,
special tokens, truncation, and media placeholder expansion. Rendering the marginal client request before resolving
continuation history is insufficient. The gateway does not implement another model tokenizer or output parser.

## Session coordinates and resolved lineage

Session metadata is a separate core contract, proposed for `types/session.rs`. It describes logical execution;
it neither asserts that KV exists nor authorizes cross-tenant sharing.

| Coordinate | Meaning and owner |
| --- | --- |
| Tenant scope | Derived from authenticated server context or an explicitly configured single-tenant domain. Client labels cannot override it. |
| Session ID | Stable policy scope for a resolved lineage, minted/bound by core and scoped to the tenant. |
| Continuation ID | An opaque, versioned logical/content position with explicit parent relationships. Its canonicalization must be agreed with llm-d; it is not a vLLM block hash. |
| Branch relationship | A child can point to a shared parent position without changing the parent's accepted history. Several branches/sessions may refer to the same exact blocks. |
| Inference-attempt ID | One model submission, unique across rounds and retries; cancellation addresses this value. |
| Public response/conversation IDs | API lookup and lineage references. They resolve to authorized history before internal coordinates are derived. |

Resolve a conversation within the tenant, then bind its stable identity to the session. For `previous_response_id`,
resolve the parent and inherit its session binding; do not use the changing predecessor ID as the stable session ID.
A first request creates a binding. Unavailable, unauthorized, or conflicting lineage fails through existing
rehydration rules; it must not become an empty prompt or a cache hit inferred from the supplied ID.

For transient `store: false` continuation, retain the binding with the existing bounded session checkpoint for the
owner's lifetime. Do not introduce a hidden database dependency. If no retained lineage or trusted binding is
available, a full stateless replay can seed a new session; content-derived estimated affinity must remain labelled as
an estimate. Persistence and restoration of a binding need an explicit schema decision in the implementation issues.

A fork creates a distinct continuation edge from the resolved parent. Compaction preserves the logical session but
creates a replacement-history position; the old prefix is not presumed reusable. Tenant changes, salt changes, and
model/frontend incompatibility require a new compatible exact-input identity even if logical lineage is retained.

`prompt_cache_key` is a client hint, not an authentication boundary or cache salt. Messages session hints from
[#159](https://github.com/vllm-project/agentic-api/issues/159) must resolve through the same tenant policy. Strip or
validate client-supplied internal session/continuation headers at the trusted edge. A worker-facing session ID carries
trusted policy coordinates; it is never inserted into block hashing to simulate cache isolation.

## Exact input, canonical keys, and residency

The exact-placement contract builds on [#69](https://github.com/vllm-project/agentic-api/issues/69) and
[#73](https://github.com/vllm-project/agentic-api/issues/73). Its evidence is separate from session policy:

```text
resolved model-visible history
  -> authoritative shared-vLLM preparation
  -> exact tokens + compatible execution metadata
  -> llm-d canonical prefix keys
  -> comparison with engine-confirmed resident extents
```

| Metadata | Why it is required |
| --- | --- |
| Exact input | Ordered rendered token IDs, effective length/truncation, special-token policy, and prefix/block boundaries. Raw messages or estimated token counts are insufficient. |
| Model/frontend identity | Immutable weights/model revision, tokenizer and vocabulary revision, renderer/template identity and options, reasoning/tool parser identity and configuration. A served alias alone is insufficient. |
| Cache isolation | Effective tenant-scoped `cache_salt`, preserved through routing and execution. A missing or changed salt cannot be silently substituted during a continuation. |
| LoRA | Selected adapter identity/revision and engine-compatible mapping. Absence is explicit. |
| Multimodal/embeddings | Prepared media or embedding identity, processor revision/options, placeholder ranges and offsets, and engine-compatible extra keys. URL equality alone does not establish content identity. |
| Cache layout | Effective block sizes and cache groups, attention/sliding-window specification, hash/key-format version, and any lookahead token requirements. |
| Execution compatibility | Weight epoch, KV dtype/layout and relevant parallelism, plus connector/tier format when moving state. Only engine-declared compatible execution can consume a block. |

The router and its KV-cache library own canonical key computation and the mapping from engine block hashes to those
keys. Agentic API passes the authoritative inputs; it must not duplicate the hash algorithm. Session/continuation
labels and public response IDs remain outside exact cache keys. Preserve cache-group and medium distinctions:
CPU/offloaded residency is not automatically immediately reusable GPU residency.

Missing metadata disables exact-locality claims. It may fall back to recomputing the complete resolved input on a
compatible worker; it may not waive required isolation or pretend an approximate match is exact. A compatible prefix
is still only a candidate until a worker reports residency, and residency can change between routing and execution.

### Residency events and restart recovery

The index identity includes the engine instance, publisher incarnation/epoch, data-parallel rank, cache group,
medium/locality, and exact block identity. A network address alone is not an engine incarnation. The event source must
also expose schema/key versions, sequence ordering, and its replay/recovery guarantees.

`BlockStored` establishes reported coverage; session attribution records which request caused the report, not
exclusive ownership. Maintain many-to-many relationships between sessions/continuations and resident blocks.
`BlockRemoved` invalidates the corresponding extent for every dependent lineage. Duplicate admissions and removals
must follow the source's declared semantics; if exact multiplicity cannot be reconstructed, conservatively invalidate
rather than retain phantom coverage. A reuse report must not be mistaken for another physical allocation.

`AllBlocksCleared`, engine replacement, publisher-epoch changes, event gaps, and uncertain reconnects invalidate the
affected coverage. Rebuild only through a verified snapshot/replay or new engine evidence. Old-epoch messages cannot
repopulate the new index. Until epoch/recovery semantics are supported end to end, treat reconnect as invalidation;
sequence regression alone cannot detect every missed restart. Losing cache evidence changes placement performance,
not the logical conversation or permission to reuse another tenant's data.

At the pinned vLLM revision, `BlockStored` has `session_id`; `BlockRemoved` is hash/group/medium based. `EventBatch`
has no publisher epoch or continuation coordinate. `Control.GetServerInfo` and `GetKvEventSources` provide useful
instance and source discovery, but do not by themselves supply the complete event-incarnation/replay contract above.

## Lifecycle signals and cache directives

Lifecycle reporting must work while no generation is active. Keep it separate from `InferenceBackend::generate` so
a tool pause, fork, compaction, or session close does not require a fake inference request. Core emits facts; llm-d
chooses policy and asks the engine to perform only supported actions.

A proposed signal envelope carries authenticated tenant/session coordinates, continuation and parent references,
a unique signal ID, per-lineage ordering/version, and an optional validity deadline. Signals describe inference start,
tool wait, resumption/next-round intent, fork, replacement after compaction, completion, and cancellation. Deduplicate
retries, reject stale updates, and expire abandoned hints. A failed notification cannot promote unaccepted history.

| Action/profile | Required contract |
| --- | --- |
| Soft affinity | Routing preference only; it can yield to load and compatible fallback. No residency or worker-survival guarantee. |
| Retention priority/TTL | Capability-gated eviction bias with bounded priority, duration, and capacity. Engine policy can evict under pressure. |
| Offload/move/prefetch/discard | Separate action capabilities, target extents/tiers and compatibility requirements; never inferred from a generic hints envelope. |
| Worker-pinned session | Separate backend-specific create/bind/close, capacity, expiry, and worker-loss semantics. A session cannot silently migrate as if it were ordinary prefix caching. |

Each directive needs an idempotency ID, target incarnation/extent, expiry, and an accepted/deferred/rejected result with
a reason and effective limits. Accepted intent is not proof of residency or completed transfer. The control plane must
handle lost acknowledgements and expire stale directives. Retention during a tool pause requires a real control path
that works without an active generation. A request-attached field alone cannot implement that behavior.

The vLLM KV hints envelope currently carries versioned action identifiers with opaque payloads. Core should expose
only agreed typed actions. The pinned chat preparation sets `kv_hints: None`; envelope availability does not establish
chat propagation or retention support. The [Session Control experiment](https://github.com/llm-d/llm-d-router/issues/2003)
is a distinct worker-pinned profile, not evidence that vLLM soft retention is implemented.

### Representative sequences

| Sequence | Required transitions and observable result |
| --- | --- |
| Built-in tool pause | Round A ends with a validated call. Core emits tool-wait for the current continuation; llm-d may request bounded retention. Core executes the tool, appends its output through the existing conversion path, then prepares round B with a fresh attempt ID and updated exact input. A denied retention request does not suppress the tool output. |
| Client-executed function | Finish the response with the call and signal awaiting client action. No generation remains open while waiting. A later request resolves the call output and parent, then starts another attempt within the same authorized lineage. |
| Continuation | Resolve `previous_response_id`, inherit the session binding, append new input, prepare exact tokens, and route from current evidence. The response ID and attempt ID change; stable session identity does not. |
| Fork | Two children resolve the same immutable parent and create separate continuation edges. Shared blocks remain many-to-many. Cancellation/eviction of one child's state cannot erase the parent's logical checkpoint or assert exclusive block ownership. |
| Compaction | Core selects the canonical compacted window and signals history replacement. Re-render and compute new exact keys; retain old-prefix coverage only where independently compatible, never by inheriting the old continuation's extent. |
| Eviction during a pause | A removal invalidates matching coverage for all dependent continuations. The next round recomputes or uses another confirmed compatible copy; successful history resolution remains independent of a cache miss. |
| Worker/publisher restart | Invalidate old-incarnation coverage and pending directives. Rediscover/rebuild evidence. An in-flight attempt fails under the cancellation/retry rules; a later attempt may recompute on another worker. A pinned-session profile instead reports its explicit loss/reopen behavior. |

## gRPC compatibility and versioning

The schemas below were inspected on 2026-11-24. These are source baselines, not a tested compatibility matrix:

- vLLM `f2f9edcac30d128bc090a3c032e4ba125716a641`:
  [inference.proto][vllm-inference], [control.proto][vllm-control], [render.proto][vllm-render],
  [chat preparation][vllm-chat], [chat events][vllm-chat-events], and [KV events][vllm-kv-events].
- llm-d-router `f075dc523b3bdc4c7f5e04c2f3afd28872a1a4d3`: [vllm_engine.proto][llmd-proto].

| Surface | vLLM Rust frontend | llm-d-router schema | Adapter requirement |
| --- | --- | --- | --- |
| Service | `vllm.Inference.Generate` and `GenerateStream`; separate `Control` and `Render` | `vllm.grpc.engine.VllmEngine.Generate` returns a stream | Select an explicit protocol; RPC names and message layouts are not interchangeable. |
| Input | `model`, text or `TokenIds`, separate sampling/decoding/stopping settings | Text or `TokenizedInput`, one `SamplingParams`, `stream` flag | Map each supported setting with presence/range checks. Model selection must be bound to the discovered endpoint. |
| Sampling | Optional `int64` seed and optional top-k/top-p/min-p | Optional `int32` seed; several scalar sampling fields have no presence | Reject unrepresentable values; do not truncate or reinterpret omitted/default values. |
| Cache/session | `cache_salt`, `session_id`, `kv_hints`, LoRA selection | No corresponding fields in `GenerateRequest` | Negotiate an actual extension or updated schema. Do not silently drop isolation/session requirements. |
| Media | `MediaItem`, including version-compatible prepared features | `MultimodalInputs` with tensors, hashes, and placeholder ranges | Require an explicit compatible conversion; the byte layouts are not equivalent. |
| Output | `PromptInfo`, `SequenceOutput`, typed `FinishInfo`; text/tokens/logprobs | Token `chunk` or `complete`, string finish reason | Preserve ordering; parse through shared vLLM output processing; handle unknown finish reasons as errors. |
| Usage | Optional final `num_cached_tokens`, prompt and output counts | Cached/prompt/completion scalar counts in chunks and completion | Identify cumulative versus incremental counters; do not sum repeated totals or treat absent data as measured zero. |
| Cancellation | `Control.Abort`; response-stream drop aborts generation | `VllmEngine.Abort` | Route abort to the same selected attempt/worker; verify delivery and cleanup end to end. |
| Discovery | Server/API/proto/instance identity, model info, KV source/rank discovery | Health, model info, and more limited server info | Supply missing negotiated metadata explicitly; health success is not compatibility evidence. |

The proposed direction is to use the vLLM-owned schema through llm-d, or an explicitly versioned llm-d adapter for a
proven compatible subset. No direct wire compatibility is assumed. Unsupported required fields fail admission;
there is no implicit downgrade to the older schema. `google.protobuf.Struct` fields in upstream protocols do not
justify an untyped core contract.

Record a tested tuple of core contract version, integration version, vLLM revision, llm-d revision, protobuf identity,
frontend/model identity, and canonical-key/event versions. Schema evolution is additive only when semantics remain
compatible: preserve field numbers/presence, reserve removed fields, and reject unknown required capabilities or
terminal meanings. Rust crate SemVer and wire protocol versions are independent. Unknown optional features remain
disabled; a version mismatch never enables a capability by inference.

Before publishing a supported profile, run request/default/usage/cancellation golden cases through both sides,
including unsupported metadata rejection. This branch supplies neither a gRPC adapter nor evidence that the two
pinned revisions can execute together.

## Validation and review record

The normalized-frame regression compares assembly, reasoning summary, usage, custom-tool translation, and gateway
sequence/offset handling against the SSE entry point. Separate boundary cases check lifecycle/terminal rejection and
retained-output limits. Existing accumulator and translator suites cover item identity, duplicate/conflicting
completion, terminal details, unfinished items, and compatibility policies. These tests establish reuse of ingestion;
they do not establish live-engine parity or distributed cache behavior. Direct boundary regressions cover
admission/drop cancellation, idle timeouts, event limits, and custom-tool session continuation through the public
executor. Native conversion tests cover request preparation, unsupported options, independent block/tool indexes,
reasoning, terminal status, and exact usage conversion through shared assembly without a running engine.

Implementation tests should extend the owning suite and compare observable results, not mirror constructors:

| Boundary | Necessary evidence before enabling it |
| --- | --- |
| #361 HTTP/typed ingestion | Equivalent public output, translation, usage, and terminal errors; malformed lifecycle/identity; existing JSON/SSE/WebSocket behavior; slow consumers, byte limits, cancellation, and no post-output replay. |
| #362 native preparation | Exact prompt and parsed-output parity for documented model profiles, reasoning and terminal-only output, client/built-in tools, multi-round usage, cancellation during preparation/submission/streaming. |
| #363 gRPC | Golden schema conversions, preserved optional settings, unsupported-field rejection, deadlines, abort of the selected attempt, bounded streaming, and dropped-worker cleanup. |
| #364 session/locality | Tenant isolation, inherited lineage, forks, compaction, shared-block eviction, duplicate/gapped events, epoch changes, stale directives, and controls while generation is idle. |
| #367 qualification | Cross-adapter conformance plus cache/latency measurements under load, tool pauses, eviction, and restarts, with pinned versions and reproducible commands. |

No new provider recordings are required merely to expose the existing normalized-event ingestion boundary. Reuse the recorded HTTP
baseline; record new or ambiguous provider behavior through the repository recorder workflow when that behavior is
implemented. A unit test of a proposed capability is not evidence that a backend supports it.

| Decision to review | Requested owners | Status |
| --- | --- | --- |
| Core-owned contract, backend injection, two native integration crates | Agentic API; design input linked above | Core boundary, native library, and composition binary implemented; cross-project agreement pending. |
| Shared preparation/parsing composition and remote-render profile | Agentic API + vLLM frontend | Pending agreement and supported model profiles. |
| Session/continuation minting, canonicalization, trust, durable/transient bindings | Agentic API + llm-d | Pending agreement; preserve exact identity separately. |
| Canonical keys, extra keys, event incarnation/gap recovery | llm-d router/KV-cache + vLLM engine | Pending agreement and conformance evidence. |
| Typed lifecycle/control actions, acknowledgements and idle-generation support | Agentic API + llm-d + vLLM | Pending agreement; hints envelope alone is insufficient. |
| gRPC schema owner, supported adapter, version negotiation | vLLM frontend + llm-d | Pending agreement; source differences recorded above. |
| Usage availability, numeric ranges, error/cleanup and retry policy | Agentic API + adapter owners | Proposed; implement and test under #361–#363. |

Issue #360 remains open until the participating projects review these decisions and the agreement is linked here.
The branch implements the contract/HTTP boundary (#360/#361), native inference (#362), and initial route/runtime
composition and unified launch (#365/#366). Distributed policy work can proceed independently; no launch command or
migration recommendation is established by this document.

[vllm-inference]: https://github.com/vllm-project/vllm/blob/f2f9edcac30d128bc090a3c032e4ba125716a641/rust/proto/inference.proto
[vllm-control]: https://github.com/vllm-project/vllm/blob/f2f9edcac30d128bc090a3c032e4ba125716a641/rust/proto/control.proto
[vllm-render]: https://github.com/vllm-project/vllm/blob/f2f9edcac30d128bc090a3c032e4ba125716a641/rust/proto/render.proto
[vllm-chat]: https://github.com/vllm-project/vllm/blob/f2f9edcac30d128bc090a3c032e4ba125716a641/rust/src/chat/src/lib.rs
[vllm-chat-events]: https://github.com/vllm-project/vllm/blob/f2f9edcac30d128bc090a3c032e4ba125716a641/rust/src/chat/src/event.rs
[vllm-kv-events]: https://github.com/vllm-project/vllm/blob/f2f9edcac30d128bc090a3c032e4ba125716a641/vllm/distributed/kv_events.py
[llmd-proto]: https://github.com/llm-d/llm-d-router/blob/f075dc523b3bdc4c7f5e04c2f3afd28872a1a4d3/pkg/epp/framework/plugins/requesthandling/parsers/vllmgrpc/api/proto/vllm_engine.proto
