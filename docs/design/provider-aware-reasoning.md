# Provider-aware reasoning replay: implementation slices

Tracking issue: [#335](https://github.com/vllm-project/agentic-api/issues/335).

## Completed foundation: typed reasoning

This slice does **not** enable opaque reasoning replay. The executor still uses the
existing vLLM replay policy: join usable plaintext content, omit summaries from the
upstream copy, and reject opaque-only continuation before normal inference.
Canonical public and persisted items retain their complete reasoning representation.

`types/io/reasoning.rs` owns the new pure wire types:

- `ReasoningTextContent` has a closed `reasoning_text` discriminator.
- `ReasoningSummaryContent` has a closed `summary_text` discriminator and string text.
- `ReasoningStatus` accepts `in_progress`, `completed`, and `incomplete`.
- `OpaqueReasoning` preserves `encrypted_content` as an exact string. It has no
  decoding or normalization operation, redacts `Debug`, and rejects values larger
  than 16 MiB of decoded UTF-8. This is a gateway ceiling, not a provider limit.

These types replace arbitrary JSON in `ReasoningOutput`. Existing Rust callers
constructing fields directly must use the new types. Missing or null `content` and
`summary` still deserialize as empty arrays; missing or null encrypted state and
status remain `None`. Ingestion retains the existing completion distinction:
omitted arrays preserve accumulated parts, while explicit null or empty arrays clear
them. Public JSON field names and valid string-valued opaque state do not change.

The existing request/body ceilings and shared retained-response budget still apply.
Reasoning parts, including empty parts, carry a structural charge; all retained text
and opaque bytes are charged through `RetainedSize`. No new queue, task, parser,
delivery path, or inference policy is introduced. The 16 MiB per-value ceiling is not
a claim about aggregate process memory; the usual retained-response budget is smaller.

Both stores now return `StorageError::InvalidHistoryItem` instead of skipping a row
that fails item decoding. Response history also rejects a missing referenced row.
This prevents legacy malformed reasoning from silently disappearing after schema
tightening. Database rows are neither rewritten nor deleted, and no SQL migration
was required for that foundation. Valid existing records keep their wire representation.
Response history references and effective metadata now also decode fallibly. Malformed
JSON, wrong field types, and explicit JSON `null` fail closed instead of becoming empty
history or default settings. SQL NULL retains its existing legacy behavior; it does not
establish replay provenance. A missing captured conversation response or a reference
to another conversation is an error when loading versioned metadata. Parse diagnostics
are intentionally excluded from storage errors because they may echo stored secrets.
These checks do not establish provider identity or authorize opaque replay.

## Completed: server policy and per-item provenance

Opaque replay remains disabled. Server configuration now accepts an explicit typed
`responses.reasoning_replay_policy`; its default and only executable value is
`vllm_plaintext`. The reserved `opaque_responses` value returns a typed core error
before rehydration, tool discovery, inference, compaction, or external commit. Startup
also rejects it before opening storage. No policy is inferred from the request model.

```toml
[responses]
reasoning_replay_policy = "vllm_plaintext"
```

`types/reasoning_replay.rs` owns the versioned `ReasoningProvenance` envelope:

- SQL NULL means unknown legacy origin. It is never backfilled or upgraded implicitly.
- `ClientSubmitted` marks manually supplied reasoning input and externally committed
  output. Receiving a successful upstream response does not upgrade those items.
- `Upstream` marks only output observed through the gateway's inference path. The
  engine attaches it after the existing JSON/SSE ingestion path has assembled output,
  before tool-round history, checkpoints, and persistence consume that output.

Each upstream observation includes its policy and a fixed-size SHA-256 identity
fingerprint. Domain-separated, fixed-width component hashes bind the configured
Responses endpoint, requested model, and an optional consistently reported terminal
model. The reported model comes from upstream metadata, separately from the public
`response.model` that the pipeline rebuilds from the request. Opaque replay also binds
the effective per-request credential (including missing vs empty). Unknown and
reported model identities hash differently. Endpoint, policy, and requested-model
changes produce distinct identities. Credential rotation changes opaque identities,
but not plaintext identities. Neither credentials nor original identity strings are
stored, and identity `Debug` output is redacted.
This is an equality fingerprint, not an authorization grant, integrity MAC, provider
attestation, or model-family compatibility claim. It observes the configured endpoint,
not any final redirect destination. Redirect policy, approved snapshots, opaque format identity, and
compatible-family rules remain enablement prerequisites.

`ReasoningOutput.replay_provenance` is skipped on both serialization and
deserialization and is absent from OpenAPI. Client or upstream JSON cannot set it.
Internal output-to-input conversion and transient session forks preserve it; manual
resubmission and the public split-execution persistence APIs always demote new output
to client-submitted origin. Existing ancestor items retain their own origin through
ephemeral-to-durable promotion and branching.

Migration `0007_reasoning_provenance.sql` adds nullable `items.reasoning_provenance`
TEXT, separately from public item JSON. It rewrites no existing data and establishes
no provenance for legacy rows. Both stores insert and restore it atomically with each
item, including batched inserts. Unknown versions/fields, malformed or oversized
envelopes (maximum 512 UTF-8 bytes), and provenance on non-reasoning items fail closed
with redacted `InvalidHistoryItem` errors. Missing provenance remains readable under
the default vLLM policy but cannot qualify opaque state for future replay.
Pre-0007 NULL-provenance reasoning rows with legacy content discriminators or
untyped state/status use a bounded compatibility projection (16 MiB JSON, at most
4096 content parts). It retains plaintext text in order, drops unsupported legacy
fields, and never infers provenance. Rows with a provenance column value do not
take this fallback and still fail closed on malformed typed data.

Supervisor-managed schemas must apply migration 0007 before this gateway starts.
Startup compatibility checks and readiness probes require the new column. Do not
drop it on rollback: older writers can leave NULL, which must remain unknown to a
future opaque profile. No public JSON field or SSE/WebSocket event is added.

Shared retained-response accounting and session checkpoint budgets charge fixed
inline provenance space for every reasoning item, even before it has an origin.
Checkpoint limits cover serialized bytes plus this fixed non-wire charge, not total
heap memory. Existing ownership, reservation/refund, cancellation and drop behavior
is unchanged. This slice adds no task, queue, parser, or client emission path.

Rust callers using struct literals must supply the new `ResponsesConfig` policy
(or use `..ResponsesConfig::default()`) and `ReasoningOutput` provenance (prefer
`ReasoningOutput::new`). The latter remains re-exported at its existing paths.
Raw storage `Item` rows now include the nullable provenance column; low-level item
insertion is crate-private so callers use typed `ResponseStore`/`ConversationStore`
operations instead of supplying arbitrary serialized SQL item data.

## Completed: upstream-reported model evidence

`types/upstream_identity.rs` defines the bounded `UpstreamModelId`, safe typed
`UpstreamModelError`, and internal `IngestedResponse` result. Model identifiers retain
their exact spelling: there is no alias resolution, normalization, or request-model
fallback. The gateway rejects empty/whitespace-only names and names exceeding 1024
decoded UTF-8 bytes. This ceiling is a gateway limit, not an OpenAI protocol limit.

JSON response bodies and SSE lifecycle response objects use the same typed model
projection. Normalization extracts SSE metadata; synchronous ingestion checks that
all supplied names agree within a round. Malformed metadata returns a redacted
`upstream_error` (HTTP 502 for blocking requests); strict ingestion also rejects a
changed name. Lenient ingestion preserves compatibility but permanently invalidates
model evidence on a conflict. The existing gateway reasoning cassette demonstrates
why: its early events report `gpt-5.6-sol`, while its terminal response echoes the
requested `gpt-5.6` alias. Neither spelling may supply model evidence for that round.
Failed ingestion does not persist a response or publish a session checkpoint. A streaming caller can already
have received earlier valid events before the final error.

Only an explicit terminal JSON status or SSE event with a model supplies evidence.
Missing/null metadata, nonterminal JSON, and lenient completion at EOF remain unknown,
even if earlier metadata or the request names a model. Strict ingestion rejects
post-terminal events as before. Lenient ingestion retains its repeated-snapshot
compatibility but permanently invalidates model evidence after any post-terminal
semantic event. This observation is not a substitute for the strict lifecycle
validation required by a future opaque profile.

One bounded model string is retained and charged once per round under the existing
shared retained-response budget; repeated matching snapshots do not double-charge.
The consuming ingestion result carries it separately to the engine, which includes
it in that round's provenance fingerprint. Inference framing and ordered delivery do
not inspect or decide model identity. No queue, worker, parser, client emission path,
database migration, or public response field is added. Public completed response
model naming remains unchanged; Rust callers constructing `EventPayload::Response`
must now supply its typed optional `model` field.

Old provenance is not rewritten: observations without a reported model retain their
original unknown-model fingerprint. A provider's self-reported name does not attest
its backend, establish compatible model families, or authorize opaque replay.
Opaque replay remains disabled. The metadata projection follows the response objects
in the [official streaming reference](https://developers.openai.com/api/reference/resources/responses/streaming-events).

## Candidate profile and compatibility preflight

The server now accepts one closed **candidate**, not an enabled capability:

```toml
[responses]
reasoning_replay_policy = "opaque_responses"
reasoning_replay_profile = "openai_gpt_5_4_2026_03_05_v1"
```

**This configuration intentionally fails startup with `OpaqueNotEnabled`.** There
is no environment variable, feature flag, or request field that bypasses the gate.
Omitting the profile under `opaque_responses` fails with `MissingProfile`; adding
one under `vllm_plaintext` fails with `UnexpectedProfile`. Default/generated
configuration remains vLLM-only. The profile's pinned model is listed in the
[official GPT-5.4 model documentation](https://developers.openai.com/api/docs/models/gpt-5.4).

`types/reasoning_profile.rs::OpaqueReasoningProfile` pins the exact endpoint
`https://api.openai.com/v1/responses`, requested model and terminal reported model
`gpt-5.4-2026-03-05`, and the Responses `reasoning.encrypted_content` contract. Its
`v1` is a gateway compatibility revision, **not** a provider encryption-format
version. No ciphertext is decoded, inspected for a format marker, or translated.
Aliases, alternate/regional endpoints, explicit port spellings, trailing slashes,
query strings, and URL credentials do not match. The strict spelling is intentional;
profile validation does not perform URL normalization or model-family inference.

`executor/replay.rs` owns the orchestration checks:

- Before rehydration, validate policy/profile consistency, exact target, and absence
  of local compaction input, triggers, or nonempty `context_management`, then enforce
  availability. Rejection precedes storage lookup and tool discovery.
- After rehydration and before each JSON/SSE inference entry point, the same
  preflight checks the canonical history against the selected profile and effective
  nonempty bearer credential, then enforces availability again. Direct unit tests and
  the core-only loopback execution fixture exercise the opaque positive path; normal
  library/server execution still stops at the earlier availability gate.
- Per-item checks reject unknown/client-submitted provenance, another policy or
  identity, missing/empty opaque state, and in-progress reasoning. Matching checks
  borrow input without cloning, mutation, filtering, or reordering. Plaintext or a
  summary cannot establish opaque compatibility.
- The engine's post-ingestion observation is fallible for a candidate opaque profile:
  every round, including rounds without reasoning, requires exact, consistently
  reported terminal model evidence before execution can advance. Reasoning items
  receive profile provenance only after that check succeeds.

Profile identities hash a separate domain, the fixed compatibility-contract domain,
and the existing routing/credential/model observation. Thus old observational
fingerprints do not qualify, even if their endpoint, model, and credential match.
Credential rotation deliberately invalidates compatibility. This fingerprint is
still an equality check, not an authorization token or provider attestation. The
target retains only a fixed-size digest and profile enum; neither credentials nor
opaque strings enter errors or `Debug`. No extra collections, queues, or tasks are
introduced. Existing provenance storage/budget size is unchanged; no migration or
legacy rewrite is needed.

Replay errors use the existing HTTP/SSE error machinery with the machine code
`reasoning_replay_incompatible`. Invalid input/model/credential combinations are
400 errors, invalid reported model evidence is a 502, and server configuration or
unavailable execution is a 500. Messages are static and redact item IDs, URLs,
credentials, model input, and opaque state. Input/model errors identify the relevant
parameter when unambiguous.

Rust struct literals for `ResponsesConfig` must include the optional profile or use
`..ResponsesConfig::default()`.

## Stateless projection and isolated transport

The candidate remains unavailable, but its adapter now has the following implemented
components. Unit tests exercise them directly; no execution feature flag bypasses
the availability gate and no live provider qualification is claimed.

- `types/upstream_input.rs::UpstreamInput` serializes a borrowed upstream-only view.
  Reasoning keeps its exact ID, summary, opaque string, optional status, and position;
  plaintext `content` and internal provenance are omitted. Absent optional fields are
  omitted, not serialized as null. Other item kinds use their existing serialization.
  The single `OutputItem::to_input_item` conversion and tool normalization remain in use.
- The candidate sends upstream `store: false`, independent of the client's gateway
  `store` choice. Default vLLM requests still omit upstream `store`. The candidate does
  not run the initial vLLM plaintext sanitizer, mutate canonical items, or discard
  opaque state. Current upstream documentation describes encrypted state as the default
  for stateless reasoning; the legacy `reasoning.encrypted_content` include remains
  accepted. No model-specific `reasoning.context` value is inferred or injected. See the
  [official stateless reasoning guide](https://developers.openai.com/api/docs/guides/reasoning).
- `executor/inference/transport.rs::ResponsesTransport` isolates the candidate from
  `ExecutionContext.client`. Its lazily initialized, shared client permits HTTPS only,
  follows no redirects, disables automatic retries and environment proxies, and has no
  caller default authorization, organization/project headers, or cookie store. Only the
  preflight-checked bearer credential is supplied per request. Connect timeout is 30 s,
  read timeout is 600 s even if the separate chunk timeout is disabled, and at most one
  idle connection is retained per host. The default vLLM and Messages clients are unchanged.
- Candidate non-2xx bodies and headers are discarded without being read or logged.
  Redirects become 502 errors, not client redirects. HTTP/SSE framing remains in
  `inference.rs`; the same framer rejects invalid UTF-8 and unfinished lines for the
  candidate while retaining the default adapter's existing compatibility behavior.
- `executor/upstream.rs` selects existing `Validation::Strict` for both candidate JSON
  and SSE; default vLLM remains lenient. Normalization, synchronous ingestion, and
  ordered delivery keep their existing owners and transitions. Invalid provider-data
  errors are wrapped with redacted Display/Debug/API diagnostics and a retained typed
  source. Source chains are for explicit internal inspection, not routine logging.
  This does not promise that valid provider-generated response text or response error
  objects are secret-free; those still require qualification through the common pipeline.
- A streamed candidate `response.failed` is logged with only the gateway response ID and
  an identifier-shaped error code. Its provider message, upstream ID, and incomplete
  reason stay out of gateway logs because they can reflect request data. Default vLLM
  failure logging is unchanged. The failure returned to the requesting client is not
  rewritten.

There is no extra queue, task, parser, lifecycle validator, or output assembly path.
Existing JSON/SSE byte limits and delivery backpressure apply. Dropping the inline
consumer drops its upstream stream; the engine now owns the producer directly as
described below. No storage migration or canonical-history rewrite is needed.

`UpstreamRequest` and `UpstreamTool` moved to `types/upstream_request.rs`, with existing
public import paths preserved. Rust struct literals must now supply `store: None` for
the default contract and convert a `Cow<ResponsesInput>` with `.into()` for `input`.
The pre-existing metadata contract was moved unchanged; no new untyped replay payload
or public protocol field is introduced.

## Stream-owned producer lifecycle

The producer abort/join gap tracked as a prerequisite with #244 is removed from the
Responses executor. `engine/streaming/producer.rs` polls the existing orchestration
future and bounded event receiver on the caller's task, rather than spawning a producer
whose `JoinHandle` is only aborted on stream drop. This applies to the default vLLM
path as well as the reserved adapter; it does not enable opaque replay.

- An unpolled stream starts no inference. Dropping it releases captured request state.
- An active or backpressured stream owns its upstream/tool futures and continuation
  lease. Drop disposes them synchronously; there is no cleanup task or unbounded reaper
  queue. While the caller is not polling, the producer makes no background progress.
- The bounded channel and configured per-event size ceiling are unchanged. Completion
  or panic drops the producer before draining accepted events in order and exposing
  exactly one outcome; no second emission or ingestion path is introduced.
- Panics become `ExecutorError::StreamProducerPanicked`, using the existing SSE error
  envelope without copying panic payloads into client diagnostics. The process-wide
  panic hook is unchanged; this is not a guarantee about arbitrary panicking tool logs.
- The engine still validates terminal event size, then persists and publishes session
  checkpoints, before emitting completion. A cancelled storage wait releases its local
  future and session reservation. No transaction-cancellation semantics were weakened.
- WebSocket transports continue to abort and join their own request tasks and retain
  session-idle fences. Those joins now also dispose of the stream's orchestration work;
  there is no nested producer task still releasing request state afterward.

These are local ownership guarantees, not rollback of remote tool effects or immediate
termination of remote model work. Shared transport/connection drivers retain their
own lifetimes. No worker-placement performance benefit is claimed; #245 measurement
work and the broader #244 delivery/observability scope remain separate.

## Pinned reference recordings and assistant phase

The recorder now supports bounded stateless item replay and independent branches for
OpenAI, vLLM and gateway targets, and honors `store: false` over WebSocket. The pinned
scenario in `tests/cassettes/record_opaque_reasoning.py` captures 18 real requests:
text and function continuations with a branch, each over JSON, SSE and WebSocket.
The six fixtures are under `tests/cassettes/reasoning/opaque/gpt-5.4-2026-03-05/`.
They use `reasoning: {effort: low, summary: concise}`, `store: false`, a 1024-output-token
ceiling, and complete item history without server-side continuation IDs. Every terminal
response reports the exact pinned model and includes opaque reasoning. The function
scenario uses automatic selection to obtain reasoning before the call: a diagnostic
forced-call capture had no reasoning in its first response and was not promoted as
evidence for that requirement.

Recordings exposed two relevant details:

- Assistant messages include `phase: final_answer`. `types/io/message.rs` now models
  optional `MessagePhase::{Commentary, FinalAnswer}` for both input and output. The
  existing typed completion, output-to-input conversion and storage path preserve it.
  Legacy absent/null phase stays absent; generated user/compaction messages never infer
  a phase. The typed executor rejects phase on non-assistant input. Old public Rust
  import paths remain available, but `InputMessage` / `OutputMessage` struct literals
  must supply `phase`; constructors default it to `None`. No database migration is
  needed because the typed item JSON already owns this field.
- Streamed `output_item.done` and `response.completed` contain distinct opaque byte
  strings. Existing ingestion retains the former; it does not replace completed items
  with terminal-envelope output. The recorder's explicit `item-done` replay source
  captures successful provider continuations using those exact bytes. No equivalence
  between encodings is inferred, no bytes are rewritten, and no second production
  ingestion or client-emission path was introduced.

The strict Rust replay test runs every captured body through `AgentPipeline` and checks
terminal model evidence, item order, opaque bytes, summaries, phase and typed request
projection. It also injects missing terminal, duplicate completion and wrong-index
faults in memory. The recorder's byte/item/turn limits, branch isolation, optional
completion source and payload-log suppression have separate offline tests. The OpenAI
Docs guidance to preserve assistant phase informed the typed contract; see the
[reasoning guide's phase section](https://developers.openai.com/api/docs/guides/reasoning#phase-parameter).

These are provider-reference recordings, not a live gateway acceptance matrix. WebSocket
recording uses independent connections and full item replay; it does not qualify the
gateway's transient connection-local checkpoint routing. The candidate remains disabled.

## Offline executor acceptance and canonical turn ordering

`executor/qualification/` replays the pinned JSON/SSE references through full
`ExecuteRequest` execution, rather than only the ingestion adapter. A crate-private
`cfg(test)` transport routes the exact pinned endpoint to a loopback socket using a
synthetic credential. Only this per-context fixture skips availability; profile,
endpoint/model, provenance and credential checks still run. It is absent from normal
library/server builds, has no environment/config/feature-flag switch, and leaves
startup, external commit and compaction gates closed. Replay responses and request
capture have explicit count/byte limits; fixture tasks are aborted and joined.

The tests exposed an ordering defect in the reserved policy's durable tool history:
gateway function calls and outputs were stored before the preceding reasoning item.
In-turn replay was correct, but later durable continuation reordered the items.
`engine/history.rs` now uses the existing canonical round-recording path for opaque
durable responses and explicit conversations as well as transient sessions. It retains
reasoning/messages/calls through `OutputItem::to_input_item`, then the existing tool
output append path records the outputs. There is no reconstruction from public MCP
projections and no second ingestion or delivery path.

The fixed-size `types/turn_history.rs::RecordedOutputPrefix` moves output-deduplication
bookkeeping from the session lease to `RequestContext`, where both storage modes can
use it. The engine alone advances the prefix; persistence retains only public output
not already represented in canonical history, plus MCP discovery metadata. Provenance,
opaque bytes and assistant phase remain on the typed items. Public response output is
unchanged. Existing default vLLM durable behavior is unchanged, as is its canonical
response-session path. No database migration or rewrite of existing history occurs.
Rust `RequestContext` struct literals must initialize `recorded_output_prefix` with
`RecordedOutputPrefix::default()`; split contexts do so and do not serialize the marker.

Coverage includes durable continuation and branching, transient forks and promotion,
origin/credential rejection before network access, unpolled/active stream drop, failed
fork isolation, missing/wrong terminal model evidence, and truncated responses. A
deterministic loopback MCP tool binds the recorded function to `lookup_code` without
editing the provider response bytes. It exercises the scheduler, two inference rounds,
one public terminal, canonical storage ordering and validation of restored provenance
for response, conversation and transient state. Failure in the second round stores no
response, but does not undo an already completed tool side effect.

The MCP declaration/normalization is a local test setup, not live provider qualification.
These are offline executor acceptance tests, not actual HTTP/WebSocket handler or live
gateway acceptance. The profile remains unavailable, and no API key is required here.

## Current slice: pinned request-surface preflight

`executor/replay/profile/parameters.rs` now validates the candidate's modeled request fields
before rehydration or tool discovery and again before each inference round, after
stored settings have been resolved. It returns a typed, redacted 400 error naming the
unsupported request parameter; values, tool names, metadata and credentials do not
enter diagnostics. This is an allowlist for this gateway profile, not a claim that
the pinned OpenAI model cannot accept other parameters.

The candidate accepts the recorded `reasoning.effort: low` and
`reasoning.summary: concise` settings (or omitted settings), `store` for gateway
persistence, `stream`, `instructions`, `max_output_tokens` from 1 to 128,000, optional
`truncation: disabled`, and optional `include: ["reasoning.encrypted_content"]`.
`parallel_tool_calls` may be omitted or `false`. Function and locally normalized MCP
declarations are allowed; MCP requires `require_approval: never`. A function tool
cannot request deferred loading or use extension fields that normalization would
drop. `tool_choice` is limited to `auto`, `none`, `required`, or a named function
without a namespace. The existing profile and provenance checks still decide
whether input item history is compatible.

The candidate rejects unqualified reasoning context, effort and summary values,
`reasoning.mode`, `reasoning.generate_summary`, other include values, `text`,
sampling overrides, `ignore_eos`, automatic truncation, metadata, enabled parallel
tool calls, `cache_salt`, and other tool kinds. Some of these are supported by the
provider but lack this gateway's qualification; some are vLLM extensions or would
lose semantics during normalization. The [official GPT-5.4 model page](https://developers.openai.com/api/docs/models/gpt-5.4)
documents the pinned snapshot and 128,000-token maximum output, while the
[reasoning guide](https://developers.openai.com/api/docs/guides/reasoning) documents
stateless encrypted reasoning and the legacy include value.

The positive recorded JSON/SSE executor matrix remains green. A new rejection
matrix covers every excluded modeled field in both request modes and confirms errors occur
before missing-history lookup or network access. The ordinary library/server
availability gate remains closed; this preflight is not profile enablement.

The selected profile has a transport-level wire guard on both HTTP and WebSocket
`response.create`. It rejects unknown and duplicate top-level request keys before
`RequestPayload` can discard them, while allowing `type`, `stream_id`, and `generate`
only in the WebSocket envelope. Its closed nested Serde sentinels admit only the
recorded user/assistant text-message, opaque reasoning, function call/output, function/MCP
declaration, and named-function-choice shapes. They reject unknown or duplicate
fields at those object levels and keep explicit item caps on input, content,
reasoning summary, and tool arrays. Function JSON Schema and MCP headers remain
open documents, but a bounded visitor rejects duplicate keys within them. Empty
captured `reasoning.content`, `output_text.annotations`, and `output_text.logprobs`
are admitted; nonempty variants still need qualification. The existing typed
`RequestPayload` remains authoritative for values and semantics. All 18 requests
in the six recorder-generated pinned reference cassettes pass this wire guard.

The executor also checks the corresponding typed input surface, including direct
core callers and effective history after rehydration: unqualified item kinds,
content parts, structured function-call outputs, namespaced calls, plaintext
reasoning content, and unexpected content extensions fail before inference. Errors
do not echo caller-supplied field names or values. HTTP `store: false` takes the
executor route whenever an opaque profile is selected, so it cannot bypass
preflight through transparent proxying. Default vLLM behavior is unchanged.
WebSocket shape failures carry no `stream_id` or `previous_response_id` into
admission: those routing fields are read only after the raw guard succeeds, so
duplicate-key requests cannot select a lane or evict a cached checkpoint.

This closes silent nested-field dropping for the candidate's currently admitted
wire shapes; it does not qualify other input item kinds, multimodal content,
nonempty metadata arrays, or arbitrary provider tool settings. The profile gate
remains closed pending live gateway and provider error-mode qualification.

## Remaining slices before enabling a provider profile

1. Complete live gateway acceptance and transport-level HTTP/WebSocket coverage, including
   connection-local routing. Offline executor acceptance now covers durable/transient
   continuations and gateway-executed tool loops, while the reference matrix covers the
   pinned provider's initial, multi-turn, function-output and branching contract, but does
   not qualify every public request parameter, tool normalization or provider error mode.
   Expand the pinned request surface only with qualification evidence before
   enabling it. Existing gpt-5.6
   recordings remain regression evidence, not evidence for the pinned model.
2. Keep local plaintext compaction distinct from provider opaque compaction. Unsupported
   combinations already fail before inference; any future expansion requires its own
   typed contract and recordings, not summary-based compatibility inference.
3. Keep live qualification opt-in and credential-local. The expanded recorder and pinned
   reference fixtures are available; future scenarios must use that workflow and staged
   validation, never hand-authored captured YAML.
4. Qualify provider `response.failed` error objects before exposing them unchanged to
   clients. Failed responses are not persisted, but the object can reflect request data.

The upstream contract requires preserving opaque state and limits reasoning reuse
to compatible model families; see the
[official reasoning guide](https://developers.openai.com/api/docs/guides/reasoning).
The gateway must not infer compatibility from the presence of `encrypted_content`.

## Verification

`reasoning_test.rs` replays the existing recorder-generated OpenAI and gateway JSON
and SSE cassettes and checks content, summaries, opaque state, status, identity, and
the output-to-input conversion. `reasoning_types_test.rs` covers schema rejection,
nullability, exact string round trips, redaction, and the decoded-byte ceiling.
Accumulator tests cover malformed strict completion and retained-budget exhaustion,
including arrays of empty typed parts. Storage tests cover invalid and missing rows;
stateful and session tests retain the existing vLLM continuation behavior.
`storage_response_integrity_test.rs` additionally verifies invalid metadata and history
references, legacy SQL NULL handling, missing/foreign captured turns, error redaction,
and refusal to persist a child of an invalid parent. A recorded initial exchange checks
that malformed continuation metadata fails before either JSON or SSE inference starts.

Captured YAML is recorder-generated. The pinned reference matrix was staged, structurally
validated and replayed through Rust before promotion; older captures were not edited.

The `reasoning_provenance_*_test.rs` suites cover policy gating, JSON/OpenAPI exclusion,
closed envelope decoding, exact opaque bytes, mixed-origin storage batches, branches,
corrupt history, and a real pre-0007 SQLite upgrade with repeated startup. Execution
tests replay existing recorder-generated Qwen and OpenAI JSON/SSE exchanges, checking
durable and transient history, cancelled forks, promotion, and external-commit
demotion. Unit tests additionally exercise fingerprint separation and non-wire budget
charges/refunds. These local replays are not live provider qualification.

`upstream_model_provenance_test.rs` replays the recorder-generated Qwen JSON/SSE
exchanges on one endpoint with an unchanged request alias. In-memory fault injection
proves that changing only the reported model changes persisted provenance, JSON/SSE
identities match, missing/null/conflicting metadata stays unknown under lenient
ingestion, and malformed terminal metadata emits an error without storing a response.
Pipeline tests cover malformed
metadata, exact UTF-8 bounds, strict/lenient terminal handling, round isolation,
retained-budget exhaustion, upstream disconnect, and client backpressure/drop.

`reasoning_profile_test.rs`, server TOML tests, and `executor/replay/profile/tests.rs`
cover closed profile selection, exact target rejection before history/tool/inference
work for JSON/SSE requests, availability gating, identity separation from legacy
observations, credential rotation, redacted error envelopes, manual/legacy rejection,
missing opaque state, compaction rejection, and unchanged input bytes/call order.
Profile observation tests prove missing or mismatched terminal evidence cannot stamp
items. These are local compatibility and fault-injection tests, not recorded OpenAI
qualification; they do not call a live provider or create captured YAML.

`types/upstream_input/tests.rs` and `executor/upstream/opaque_tests.rs` verify stateless
projection, optional-field omission, unchanged canonical history, default vLLM behavior,
and replay of recorded OpenAI JSON/SSE through the existing strict ingestion path.
In-memory faults cover invalid reasoning, missing terminals, duplicate completions,
out-of-order completion, index mismatch, and disconnects. A bounded delivery test checks
backpressure and dropping inline input on cancellation or client disconnect. Transport
tests use loopback fixtures (HTTPS disabled only in those fixtures) for redirect rejection,
redacted HTTP errors, explicit headers, byte limits, read timeouts, and invalid/truncated
UTF-8 framing; the production constructor rejects HTTP before contacting the fixture.

`engine/streaming/producer/tests.rs` checks unpolled, suspended and backpressured
producer disposal, no background progress, ordered queue draining, closed-channel
completion, and panic isolation. `stream_producer_lifecycle_test.rs` replays existing
Qwen reasoning and vLLM tool-call cassettes to check immediate session/tool cleanup,
panic event numbering and redaction, cancellation during storage waits, unchanged
persistence-before-completion, and retention of a source checkpoint after a cancelled
fork. No captured YAML was added or modified for this lifecycle change.

The workspace suite (including OpenAPI and cassette tests), Clippy with warnings
denied, and formatting checks passed with Rust 1.98. Opt-in ignored tests were not run:

```bash
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
```

Rust 1.85 verification currently stops at dependency MSRV checks: the existing
lockfile includes dependencies requiring Rust 1.86–1.88. This slice does not change
upstream dependency versions; `sha2` 0.10.9 was already locked and is now also a direct
core dependency. Dependency and baseline language compatibility need a separate
MSRV repair before the repository can claim the documented 1.85 release gate.
