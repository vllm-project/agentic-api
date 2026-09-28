# Shared Responses library boundary

## Status

The initial `crates/vllm-responses` package owns typed semantic inference
events and is consumed by Agentic API response assembly. The remaining request,
capability, cancellation, and streaming traits below are the next contract
increments for Agentic API issues #360, #361, and #362.

## Purpose

`vllm-responses` will be a storage-free Rust library that owns the
transport-independent portion of the Responses API. Both the Agentic API
server and the vLLM Rust frontend will call it. It must support a stateless
configuration without initializing a database or gateway-executed tools.

The library owns public Responses request validation, output-item lifecycle,
response finalization, and conversion between semantic inference events and
Responses payloads. Agentic API's full runtime remains responsible for
rehydration, persistence, client delivery, and the tool loop.

vLLM remains responsible for model-specific prompt rendering, tokenization,
multimodal preparation, decoding, reasoning parsing, tool-call parsing, and
EngineCore communication.

## Inference contract

The library exposes a typed `ResponsesInference` trait. Implementations
receive a resolved, model-visible request and return a bounded stream of
semantic inference events plus one terminal outcome.

```rust
pub trait ResponsesInference: Send + Sync {
    fn start(
        &self,
        run: ResponseRun,
        cancellation: CancellationToken,
    ) -> Result<InferenceEventStream, InferenceError>;

    fn capabilities(&self) -> InferenceCapabilities;
}
```

`ResponseRun` carries the caller-assigned public response ID and the bounded
response budget. Agentic API assigns that ID when it must preserve response
lineage; a stateless vLLM route assigns one for its own response. The model
adapter never derives public IDs from engine request IDs.

`PreparedInferenceRequest` contains typed resolved messages, canonical
function tools, sampling settings, model/frontend identity, declared session
intent, and an explicit deadline. It does not contain a JSON request body, SSE
lines, a public response ID, storage state, or a cache-residency assertion.

The initial native profile is deliberately narrower than the full Agentic API:
single-part text, single-part reasoning, function calls, usage, cancellation,
and a terminal outcome.
Gateway-executed built-in tools, MCP discovery, compaction, and persistence
remain Agentic API concerns. An adapter must advertise unsupported features in
`InferenceCapabilities` and reject them before inference rather than silently
dropping them.

`InferenceEvent` represents model semantics, not wire framing: response
acceptance, text and reasoning deltas, function-call lifecycle, usage, and one
terminal outcome. It retains stable model item identities and ordering. The
contract uses typed enums and structs; `serde_json::Value` is not an API
boundary substitute.

The stream is bounded by the caller-owned response budget. An implementation
must stop promptly when its cancellation token is cancelled. It may not retry a
request after it has emitted an event without an explicit retry policy owned by
the caller.

The shared contract must not expose vLLM's current untyped
`kv_transfer_params` or `ec_transfer_params` values. Exact renderer identity,
cache block identity, cache residency, and any cache-control action remain
subject to the joint #360 contract with vLLM and llm-d. Until then, the native
profile advertises no cache-transfer capability; a session label alone does not
establish placement or cache compatibility.

## Adapters

The first two adapters implement the same trait:

- `HttpResponsesInference` sends an HTTP request, frames SSE bytes, and passes
  each decoded event through the shared event normalizer.
- `VllmChatInference` lowers `PreparedInferenceRequest` into
  `vllm_chat::ChatRequest`, consumes `ChatEventStream`, and emits semantic
  `InferenceEvent` values directly.

The native adapter must not serialize `ChatEvent` into an SSE string or parse
it again. The HTTP adapter is the only owner of HTTP I/O and SSE framing.

### vLLM `ChatEvent` mapping

| vLLM event | Shared semantic event | Contract requirement |
| --- | --- | --- |
| `Start` | response accepted / in-progress | The `ResponseRun` caller owns the public response ID; the adapter retains vLLM prompt metadata only when a declared capability needs it. |
| `BlockStart`, `BlockDelta`, `BlockEnd` for text or reasoning | output-item and content-part lifecycle plus deltas | The adapter owns stable item IDs and converts `usize` block indexes to checked Responses output indexes. |
| `ToolCallStart`, `ToolCallArgumentsDelta`, `ToolCallEnd` | function-call lifecycle and argument deltas | The adapter preserves the vLLM call ID and name, and must retain argument order across parallel calls. |
| `Done` | one terminal outcome with usage | The adapter reconciles the assembled assistant message with streamed item identities before terminal delivery. |
| `LogprobsDelta` | capability-gated metadata | It is not silently discarded when a client has requested it; the initial profile reports it unsupported. |
| stream error or cancellation | typed inference failure or cancellation | Agentic API converts it to the public failure lifecycle; it must not fabricate completed output items. |

`ChatRequest::intermediate = false` is valid for vLLM, so the adapter must
also support terminal-only generation: it derives complete output items from
the `Done` assistant message rather than requiring earlier deltas. The vLLM
event contract uses `usize` indexes while Responses uses bounded wire indexes;
an out-of-range conversion is an adapter failure, never truncation.

`Start` maps to the ordered `response.created` and `response.in_progress`
semantic events. A native adapter must emit both with the `ResponseRun` public
response ID before it emits output-item events.

## Ownership

```text
vLLM ChatLlm ──typed model events──┐
HTTP/SSE upstream ──normalized events┤
                                     ▼
                          vllm-responses ingestion
                                     ▼
                         Responses output lifecycle
                           ├─ stateless vLLM route
                           └─ Agentic orchestration
                                ├─ tools
                                ├─ persistence
                                └─ client delivery
```

The shared library does not execute gateway-executed built-in tools, resolve
continuation history, persist responses, assign client SSE sequence numbers, or
manage client connections. Those operations remain in Agentic API's executor.
The stateless vLLM route supplies a no-storage configuration and exposes only
the capabilities it can honor.

## Compatibility and failure rules

- Capabilities are explicit. Unsupported model-visible behavior fails before
  inference; output presentation controls may be accepted only when their
  documented no-op behavior is compatible with the selected profile.
- Public response IDs, session identifiers, and exact renderer/tokenizer/cache
  identity remain distinct values.
- Terminal events are response-level state only. Before `Incomplete` or
  `Failed`, an adapter must emit a completed event for every active output
  item; it must not silently discard or invent partial output. An adapter that
  cannot satisfy that invariant returns an inference error instead.
- The same semantic event sequence must produce the same public response under
  HTTP and native adapters for a declared capability profile.

## Migration order

1. Extract typed request, capability, cancellation, and terminal-outcome
   contracts into `vllm-responses`; typed semantic inference events are
   already present.
2. Route Agentic API's current HTTP adapter through the contract while retaining
   the existing ingestion and delivery state transitions.
3. Replace the executor-internal pre-normalized `EventFrame` seam with the
   shared typed-event entry point and prove HTTP/native parity with
   fixtures, including lifecycle-invalid streams and terminal failures.
4. Implement `VllmChatInference` in vLLM using `ChatLlm`.
5. Move the stateless vLLM Responses route to the shared library, then gate
   Agentic-only storage and tool-loop behavior by profile capabilities.
