# Server-Managed Prompt Cache State for the Messages API

## Summary

vLLM Agentic API should manage private server-side state for Anthropic-compatible Messages prompt caching. The state
should improve prefix-cache reuse across otherwise stateless `/v1/messages` requests by coordinating worker affinity,
cache lifetime, branch lineage, and provider-specific cache handles.

The public Messages contract does not become stateful. Clients continue to send the complete system prompt, tools, and
message history on every request. Server-managed state is advisory: if it is missing, stale, incompatible, corrupt, or
unavailable, the gateway sends the complete request normally. Cache state must never be required to reconstruct model
input.

The internal architecture should use the same structural pattern as Responses state management:

```text
scope -> checkpoint -> parent/branch -> execution metadata -> attachment -> lifecycle
```

The semantics differ. Responses state can be public and authoritative: a previous response or conversation may supply
model-visible input omitted by the caller. Messages cache state is private and non-authoritative: it may optimize a
complete request but cannot supply omitted conversation content. The implementation should share protocol-neutral
storage and lifecycle mechanisms with Responses while keeping records, public identifiers, request preparation, commit
rules, and failure behavior separate.

Delivery is incremental:

1. Preserve and observe `cache_control` without storing cache checkpoints.
2. Add tenant-safe session affinity.
3. Persist immutable cache checkpoints when a routing, retention, or provider-handle consumer exists.
4. Add capacity-aware retention hints where supported.
5. Add portable token-prefix artifacts only after exact rendering and compatibility have been proven.

## Context

### Messages prompt caching

Anthropic Messages requests can express prompt-cache intent in two ways:

- top-level automatic `cache_control`; and
- explicit `cache_control` markers on eligible tool, system, and message content blocks.

Explicit markers identify cacheable prefixes in protocol order: tools, then system, then messages. One request may
contain several nested breakpoints with independent lifetimes. The currently supported lifetimes are five minutes and
one hour. An ephemeral marker with no `ttl` means five minutes.

Prompt caching is prefix-based. Reuse depends on the exact rendered prefix and compatible model execution. A stable
session identifier is useful for routing, but it does not prove that two requests have the same token prefix.

### Current Agentic API behavior

The Messages handler and executor operate on raw JSON. This preserves Anthropic fields and content-block variants that
are not represented by the Responses types. Direct requests are forwarded to `/v1/messages`; gateway-executed built-in
tools may create additional inference rounds by appending assistant `tool_use` and user `tool_result` blocks internally.

Supporting `cache_control` at the transport layer is necessary, but forwarding markers alone leaves cache performance
to upstream load balancing and eviction policy. Agentic API currently has no Messages-specific state model for keeping
successive requests on compatible workers, representing cache branches, or retaining provider cache handles.

### Relationship to the existing Messages design

The existing Claude Code integration design deliberately rejected server-managed conversation state for Messages.
That decision remains in force: the client sends every model-visible input, the gateway does not rehydrate omitted
history, and `/v1/messages` exposes no continuation object or identifier.

This RFC narrows the older document's broader statements that the gateway keeps "no state at all." It introduces only
private, non-authoritative cache optimization state that can be discarded without changing the request or response.
The new state cannot supply model-visible input, authorize a continuation, or make a cache hit part of API correctness.
In this document, "stateless Messages API" therefore means conversation-stateless at the public protocol boundary, not
that the implementation is prohibited from retaining advisory execution metadata.

### Existing Responses state management

Responses already demonstrates the relevant internal shape:

- `conversations` provide a durable scope;
- `responses` are checkpoints;
- `previous_response_id` represents parent lineage;
- `history_item_ids` identify protocol-owned state;
- stored metadata retains effective execution settings;
- rehydration prepares a later request; and
- persistence records an accepted inference boundary.

Messages needs analogous structural primitives, but it must not use Responses rows or continuation semantics. A stored
response owns model-visible item history. A Messages cache checkpoint owns only optimization metadata and optional
provider artifacts.

## Evidence and motivation

### Claude Code client behavior

An isolated capture of Claude Code 2.1.238 against a local synthetic Messages SSE endpoint observed the following:

- every turn sent a complete streaming Messages request;
- resumed turns resent the full conversation and retained `x-claude-code-session-id`;
- default API-key requests emitted explicit ephemeral markers with an omitted five-minute TTL;
- `ENABLE_PROMPT_CACHING_1H=1` emitted explicit `ttl: "1h"` markers through a custom base URL;
- `FORCE_PROMPT_CACHING_5M=1` overrode the one-hour setting;
- `DISABLE_PROMPT_CACHING=1` removed the markers;
- marker count and placement changed with client surface and dynamic-system-prompt settings;
- the normal tool surface sent tool definitions without putting `cache_control` on a tool;
- the newest conversation marker moved as history grew; and
- an attribution block was stable in one client surface but changed across a resumed bare-client session.

The capture pointed a custom Claude Code base URL at a local synthetic SSE endpoint, recorded request headers and JSON,
and compared fresh, resumed, and environment-flag variants. It used synthetic credentials and did not establish real
provider cache hits. The raw capture is not part of this PR, so these observations are supporting evidence rather than
normative compatibility requirements. Phase 0 must regenerate and check in versioned fixtures before relying on them as
a release gate.

These observations imply that the server can use stable session and agent coordinates for affinity, but it must derive
cache applicability from the complete current prefix. It cannot hard-code a marker count, marker location, or globally
stable attribution prefix.

Claude Code documentation provides additional signals: model, effort level, fast mode, tools, Model Context Protocol
(MCP) configuration, plugins, compaction, and client upgrades can invalidate cache prefixes; subagents use separate
cache contexts; forks may reuse a parent prefix; and workflow fan-out deliberately warms a shared prefix before
followers proceed.

### Affine KV-cache evidence

The companion [affine KV continuation draft ADR](https://github.com/vllm-project/agentic-api/blob/feat/adr04-token-cache-phase2/docs/adr/ADR-04_kv_affine_continuation.md)
and its [raw results](https://github.com/vllm-project/agentic-api/tree/feat/adr04-token-cache-phase2/docs/adr/results/adr-04/2026-07-13-n12)
show why routing and state coordination matter. The controlled benchmark used 12 sequential two-turn pairs per routing
profile. Load-only continuation achieved approximately 49.5% mean KV hit rate and 811 ms mean latency, while precise or
approximate continuation achieved approximately 99.1% and 315 ms. This is a directional single-client routing proof,
not a production guarantee: it did not cover multi-tenant traffic, branches, worker churn, cache pressure, or long
pauses, and it does not establish an expected Phase 2 improvement for Messages.

Together, the client and engine evidence support server-managed execution state, not server-managed Messages history.
The opportunity is to preserve cache locality and compatibility while retaining a full-request fallback.

## Problem statement

Forwarded `cache_control` expresses client intent but does not coordinate the infrastructure that determines reuse:

- successive requests may be routed to different workers;
- worker-local KV state may be evicted before the requested TTL;
- several breakpoints in one request may have different lifetimes;
- a client session can branch through concurrent requests, subagents, forks, and retries;
- hidden gateway tool rounds create prefixes not directly represented by a later client request;
- model, tokenizer, rendering, tool, effort, or cache-salt changes make state incompatible;
- provider cache handles may be worker-bound, expiring, or replay-sensitive; and
- client intent metrics can look healthy even when no cache read occurred.

The system needs an internal model that answers four questions safely:

1. Which routing scope should this request prefer?
2. Is any stored checkpoint compatible with the complete current request?
3. Which cache extents or provider handles are still usable?
4. What state, if any, should be recorded after this inference boundary?

## Goals

- Preserve client-supplied Messages JSON, headers, `cache_control`, and unknown fields through direct and tool-loop
  paths.
- Keep `/v1/messages` publicly stateless and require complete client input on every turn.
- Derive tenant-safe session affinity from authenticated identity and bounded client coordinates.
- Represent branching checkpoints and multiple independently expiring cache extents.
- Reject reuse across incompatible models, renderers, tools, effort modes, salts, tenants, or upstream pools.
- Share proven storage and lifecycle mechanisms with Responses without sharing protocol records or semantics.
- Make every optimization degrade to an ordinary full-request inference.
- Separate cache intent, routing success, cache creation, and cache reads in observability.
- Bound storage, worker skew, retention, and cleanup cost.
- Support staged rollout and rollback without requiring a client migration.

## Non-goals

- Adding `previous_message_id`, `previous_response_id`, or a public Messages conversation object.
- Reconstructing omitted Messages history from server state.
- Copying complete Messages transcripts into cache tables.
- Storing Messages cache state in `conversations`, `responses`, or `items`.
- Guaranteeing KV residency for the requested TTL.
- Fabricating Anthropic cache usage when the upstream does not report it.
- Replaying gateway-executed tools from cache after process failure.
- Persisting raw API keys, authorization headers, or unredacted client session identifiers.
- Storing engine-specific KV tensors in the initial implementation.
- Creating one generic state handler controlled by runtime `authoritative` or `fail_open` flags.

## State semantics

Responses and Messages share an architectural pattern but have different contracts:

| Dimension | Responses | Messages cache state |
|---|---|---|
| Public API | Stateful continuation is exposed | No state object or continuation field is exposed |
| Input authority | Stored items may supply omitted context | Complete client body remains authoritative |
| Checkpoint | Stored response | Accepted cache-prefix observation |
| Scope | Conversation or response chain | Internal tenant-scoped cache session |
| Parent | Public previous response or conversation order | Internal prefix-compatible branch |
| Payload | Model-visible items and effective settings | Prefix identities, extents, receipts, and artifacts |
| Read failure | Required state failure rejects continuation | Cache failure bypasses optimization |
| Write failure | Can fail requested storage semantics | Must not fail an otherwise successful Messages response |
| Identifier | Public response/conversation ID | Internal, tenant-bound identifier |

The shared implementation boundary is mechanical, not semantic. Both projections may reuse the SQLx pool,
transactions, clocks, ID generation, immutable-parent validation, idempotent insertion, lifecycle versions, quotas,
encryption, cleanup, and repository tests. They retain separate schemas, stores, payload types, request preparation,
accepted-boundary predicates, and error policies.

## Proposed architecture

```text
POST /v1/messages
        |
        v
raw request + validated transport context
        |
        +--> CacheIntentParser
        +--> CacheScopeResolver
        +--> ExecutionFingerprintBuilder
        |
        v
MessagesCacheCoordinator
        |
        +--> MessagesCacheStore -------- immutable checkpoints/extents
        +--> PrefixMatcher ------------- exact applicability
        +--> CacheRoutePlanner --------- route intent/capability
        +--> ProviderCacheAdapter ------ receipts/retention/artifacts
        |
        v
Messages executor and gateway tool loop
        |
        v
llm-d router or direct vLLM upstream
        |
        v
usage/receipt observation + accepted-boundary commit
        |
        v
unchanged client response
```

### Control-plane and inference boundary

Server-managed cache state is normalized into provider-neutral KV lifecycle hints before a provider adapter maps it to
an upstream-specific request. This keeps logical state and policy in Agentic API while leaving physical cache ownership
with the inference system.

Agentic API owns:

- authenticated tenant scope and opaque engine-facing session and continuation coordinates;
- durable logical lineage, compatibility fingerprints, desired lifecycle, and expiration;
- mappings from logical checkpoints to provider receipts, artifacts, worker observations, and engine epochs; and
- route intent, retain, offload, prefetch, evict, and fallback policy based on application lifecycle.

In an llm-d deployment, Agentic API sends the complete prepared request plus trusted coordinates and lifecycle hints to
the llm-d inference endpoint. llm-d owns its worker-membership view, exact-prefix index, load-aware scoring, and vLLM
endpoint selection. Agentic API must not duplicate llm-d's canonical block-key algorithm or claim exact residency from
a session coordinate. In a direct deployment without llm-d, a provider adapter may perform targeted routing when the
upstream exposes that capability.

vLLM and its KV connectors own:

- mapping opaque coordinates to current block hashes, physical blocks, and connector keys;
- validating that mapped content is still current before acting on a hint;
- applying soft retention, offload, prefetch, and eviction under health and capacity constraints; and
- reporting observed placement, cache outcomes, and an epoch that changes when engine-local mappings are lost.

Clients never receive or manage engine block identifiers, connector keys, or cache handles. Anthropic `cache_control`
remains protocol-level cache intent; it is one input to Agentic API policy, not an engine pointer. A worker restart
invalidates engine-local observations for the old epoch. Durable control-plane state can then choose another compatible
route, recover a remote artifact, or fall back to the complete request, but it cannot treat lost worker-local KV as
durable data.

The vLLM session-coordinate and agent-hint proposals are complementary to this boundary: typed coordinates identify
logical state without becoming cache keys, while lifecycle hints tell vLLM how to treat the physical KV associated with
those coordinates. Agentic API should mint both from its internal state instead of exposing either mechanism as a new
Messages client contract.

### `CacheIntentParser`

Reads cache policy from raw Messages JSON without rewriting the request. It produces ordered normalized extents and an
opaque classification for unsupported future forms. Unknown forms pass through unchanged and disable only the
optimization Agentic API cannot understand.

Automatic top-level caching expresses policy without naming a source block. Until an exact renderer or trusted provider
result identifies the covered boundary, automatic mode may drive observation and session affinity but cannot create an
exact extent, portable artifact, or strong reuse claim.

### `CacheScopeResolver`

Resolves authenticated tenant scope, upstream pool, optional client session coordinate, agent routing lane, and stable
cache salt. A client header never establishes tenancy. Raw client coordinates are validated, transformed with a
versioned HMAC, and excluded from logs and durable rows.

### `ExecutionFingerprintBuilder`

Builds a versioned digest over every known compatibility input:

- upstream pool, model identifier, and immutable model revision where available;
- tokenizer and Messages rendering-template revisions;
- Messages protocol version and enabled beta capabilities when they affect rendering or execution;
- normalized tool schema and effective tool inventory;
- thinking/reasoning configuration, effort level, and fast/speed mode;
- relevant multimodal processor configuration;
- cache-salt identifier and version; and
- fingerprint schema version.

Unknown compatibility inputs permit session affinity but make receipts and artifacts ineligible.

### `PrefixMatcher`

Determines whether a stored extent applies to the complete current request. Its result is one of:

- `exact_prefix`: strong state may be reused;
- `not_applicable`: the candidate must be ignored; or
- `unknown`: affinity is allowed, but no receipt, retention claim, or artifact may be attached.

Exact proof comes from a canonical token/render digest or a trusted provider-signed covered-prefix claim. Session ID,
agent ID, source position, timestamp, or an unsalted JSON hash is insufficient.

A provider handle must declare whether it covers only the request's `input_prefix` or a
`post_generation_continuation`. A post-generation handle is applicable to a later request only when the matcher proves
that the accepted assistant output and every intervening client block appear exactly in the current rendered prefix.

### `CacheRoutePlanner`

Chooses a preferred route using tenant scope, session tag, agent lane, candidate compatibility, worker health, and load.
Affinity is subordinate to health and admission control. If the configured upstream cannot target a worker or express
provider-owned affinity, route mode is rejected at startup rather than silently doing nothing.

### `MessagesCacheStore`

Owns Messages-specific sessions, checkpoints, extents, and artifacts. It uses shared storage/lifecycle helpers but does
not call `ResponseStore` or `ConversationStore`. Responses handlers cannot query cache tables, and cache identifiers
cannot be resolved through public response or conversation endpoints.

### `ProviderCacheAdapter`

Declares and implements upstream-specific capabilities:

```text
accepts_cache_control: yes | no
session_affinity: none | header | body | targeted_route
continuation_receipts: none | signed_opaque
retention_hints: none | soft_ttl
cache_salt: none | body
token_prefix_artifacts: none | versioned
```

Capabilities are configured and validated explicitly. They are not inferred from a model name.

## State model

All tables use the existing SQLx `Any` pool and support SQLite and PostgreSQL. Messages tables have independent roots
and use internal generated IDs. Durable state is enabled only when a configured consumer can use it.

### `messages_cache_sessions`

One row identifies a tenant-scoped routing and lifecycle coordinate:

- internal `id`;
- `tenant_scope` and tenant-key version;
- HMAC-derived `session_tag` and session-key version;
- `upstream_pool`;
- cache-salt identifier and version;
- lifecycle status: `active`, `expired`, or `revoked`;
- optimistic lifecycle version; and
- `created_at`, rate-limited `last_seen_at`, and `expires_at`.

`(tenant_scope, session_tag, upstream_pool)` is unique. Session state is not cache identity and does not authorize prefix
reuse.

### `messages_cache_checkpoints`

One immutable row records an accepted inference observation:

- internal `id` and `session_id`;
- nullable `parent_checkpoint_id`;
- request ID and inference-round index;
- execution fingerprint and schema version;
- request-prefix digest when exact identity is available;
- accepted assistant-output digest for branch matching, when available;
- acceptance level: `observed`, `receipt_verified`, or `artifact_verified`;
- deduplication identity;
- `created_at` and expiry derived from usable extents; and
- bounded sanitized diagnostics.

A checkpoint never changes parent or prefix. Concurrent children represent branches. `observed` checkpoints support
metrics and lineage but do not authorize strong reuse.

### `messages_cache_extents`

One row records one ordered cache breakpoint:

- checkpoint ID and zero-based ordinal;
- normalized source kind and diagnostic locator;
- requested TTL and calculated expiry;
- coverage kind: `input_prefix` or `post_generation_continuation`;
- exact prefix digest when known;
- covered token count when reported by a trusted source;
- optional provider receipt reference and expiry;
- optional token-artifact reference;
- state: `intent_only`, `resident_hint`, `receipt_verified`, `artifact_verified`, `expired`, or `invalidated`; and
- invalidation reason.

The initial limit is four extents per checkpoint. Marker order and mixed lifetimes are preserved.

### `messages_cache_artifacts`

Portable artifacts are optional and deferred. An artifact row contains:

- tenant and execution-fingerprint binding;
- encrypted token-prefix payload or provider-defined portable representation;
- checksum, byte length, format version, and encryption-key version;
- covered-prefix digest;
- creation and expiry times; and
- validation status.

Artifacts contain no reusable authorization credential. Corrupt, unrecognized, or incompatible artifacts are
quarantined and bypassed.

## Request lifecycle

### 1. Prepare

1. Parse and size-limit the raw request.
2. Extract cache intent without mutation.
3. Resolve authenticated tenant scope, session tag, agent lane, cache salt, and execution fingerprint.
4. Load upstream cache capabilities.
5. Select bounded, unexpired candidates for the same tenant, pool, salt, and fingerprint.
6. Use session-compatible candidates for affinity only.
7. Require `exact_prefix` before attaching a receipt, retention decision, or artifact.
8. Produce a route and optimization plan.

Any cache-state read failure records a fallback reason and continues with the complete request. Malformed or forged
client state that represents an active security violation is rejected rather than silently accepted.

### 2. Infer

The complete raw client body is forwarded. Provider adapters may add out-of-band affinity, receipt, or retention hints
only when the capability is declared. They do not add, move, or remove client `cache_control` markers.

For gateway-executed built-in tools, hidden rounds use the same scope and fingerprint but keep round state in memory.
Receipts from a hidden round may accelerate the next round in the same request. They are not durable continuation
points and are discarded if the outer turn does not reach an accepted terminal boundary.

### 3. Observe

Capture separately:

- normalized client cache intent;
- chosen route and whether affinity was achieved;
- provider receipt or retention outcome;
- upstream `cache_creation_input_tokens`;
- upstream `cache_read_input_tokens`; and
- fallback or invalidation reason.

Agentic API never infers a cache hit from marker presence, route affinity, latency, or a prior checkpoint.

### 4. Commit

A durable checkpoint is eligible only after:

- a structurally valid non-streaming response;
- a complete direct stream ending in a valid `message_stop`; or
- the final accepted outer boundary of a gateway tool loop.

Upstream errors, malformed streams, timeouts, cancellation, and exhausted tool-round budgets create no durable accepted
checkpoint for that boundary.

Messages commits are bounded and best-effort. A successful client response is not delayed or changed by a cache-store
failure. A self-contained queue record includes the expected tenant, session, salt, and fingerprint versions; the writer
revalidates revocation and expiry before inserting. Queue saturation drops the optimization write rather than applying
unbounded inference backpressure.

## Concurrency and branching

- Requests sharing a session tag may execute concurrently.
- Several children may reference the same compatible parent.
- There is no mutable “latest checkpoint” pointer.
- Completion order does not determine authority.
- Duplicate checkpoint commits converge through a versioned deduplication key.
- Candidate selection is deterministic: compatible, unexpired, strongest verification, longest exact prefix, newest
  creation time, then stable internal ID.
- Agent and parent-agent headers are routing-lane hints, not principals or parent-proof.
- Side-effecting built-in tools are never replayed as cache recovery.

## Routing and cache retention

`CacheRoutePlanner` produces a capability-checked route intent. With llm-d, the adapter forwards trusted session and
continuation coordinates after Messages request preparation; llm-d combines them with exact-prefix evidence, worker
health, load, and admission control to choose the vLLM endpoint. The planner does not reproduce llm-d tokenization,
canonical block keys, event indexing, or scoring.

Without llm-d, a direct provider adapter may use a versioned worker-membership view and deterministic placement when
the upstream supports an explicit route target. Rendezvous hashing or an upstream equivalent may use
`(upstream_pool, tenant_scope, session_tag, optional_agent_lane)`. If neither llm-d nor the direct upstream exposes an
effective routing capability, `route` mode is rejected at startup.

Worker membership changes can remap a session. Remapping is a cache miss, not a request failure. Affinity cannot override
health, load shedding, or admission control.

Requested TTL is a retention preference bounded by provider support, available capacity, and operator policy. It is not
a pin or residency guarantee. Retention hints use remaining extent lifetime and are disabled when the upstream lacks an
explicit compatible capability.

Provider receipts are reusable only when their issuer and schema are trusted and they bind at least:

- tenant scope and session tag;
- upstream pool and execution fingerprint;
- covered-prefix digest and coverage kind;
- issue and expiry times;
- unique receipt ID or nonce;
- receipt schema and signing-key versions; and
- an integrity signature.

Any missing claim, signature failure, replay, expiry, or binding mismatch bypasses the receipt and sends the complete
request. A receipt indicating cross-tenant tampering is rejected and audited.

## Security and privacy

### Threats

- cross-tenant prefix reuse can disclose prompt content through output or timing;
- raw session headers may contain user or workspace identifiers;
- attackers can create high-cardinality sessions and one-hour extents;
- receipts can be forged or replayed across tenants, models, or workers;
- artifacts can contain sensitive tokenized prompt material;
- debug logs and metrics can accidentally expose content or stable identifiers; and
- asynchronous writers can recreate state after revocation or deletion begins.

### Controls

- authenticated principal resolution precedes all state lookup;
- session tags and prefix identities use domain-separated versioned HMACs;
- raw session and agent coordinates are neither logged nor stored;
- state, salts, receipts, and artifacts are tenant- and upstream-bound;
- per-tenant and global quotas bound sessions, branches, extents, artifacts, and one-hour retention;
- receipts and artifacts are encrypted at rest when content-bearing;
- key rotation makes old material ineligible and supports bounded retiring-key cleanup;
- deletion revokes the session first and removes dependent rows idempotently;
- asynchronous writers revalidate lifecycle versions before insert; and
- ordinary diagnostics expose truncated opaque identifiers, not content, raw digests, or artifacts.

## Failure behavior

| Failure | Behavior |
|---|---|
| No client session coordinate | Process normally; record cache intent; do not assume cross-request continuity |
| Cache database unavailable | Bypass stored candidates; keep full-request inference available |
| Worker remapped or cache evicted | Send the already-complete request and record a cache miss |
| Fingerprint or prefix mismatch | Do not attach receipt or artifact |
| Unknown `cache_control` form | Pass through unchanged; classify as unsupported optimization |
| Expired extent or receipt | Ignore it and continue normally |
| Corrupt artifact | Quarantine it and perform a full render |
| Forged or cross-tenant receipt | Reject the supplied state and emit a security event |
| Commit queue full | Drop the cache checkpoint write; do not block inference |
| Commit fails after response | Preserve the client response; record lost optimization |
| Stream ends without valid terminal event | Commit no accepted checkpoint |
| Upstream rejects a breakpoint | Preserve the upstream error so the client can perform its documented retry |

Responses retains its existing failure semantics. Shared helpers must not convert a missing required previous response
into Messages-style fail-open behavior, nor make Messages cache persistence a prerequisite for request success.

## Observability

Metrics must distinguish:

- requests containing cache intent by form and TTL;
- route-affinity attempts, successes, remaps, and load overrides;
- exact-prefix, mismatch, and unknown applicability results;
- upstream cache creation and read tokens;
- checkpoint candidates, commits, deduplications, drops, and failures;
- receipt and artifact acceptance or rejection reasons;
- active sessions, checkpoints, extents, and artifact bytes;
- cleanup lag, expiry lag, and quota pressure; and
- fallback reasons by upstream pool and client class.

Traces may include request ID, truncated HMAC-derived session tag, fingerprint version, route decision, and fallback
reason. They exclude prompt content, raw client coordinates, credentials, full digests, receipts, and artifacts.

## Capacity and lifecycle management

Configuration must bound:

- active sessions per tenant and globally;
- checkpoints per session and branch fan-out;
- extents per checkpoint;
- accepted one-hour extents;
- artifact count and bytes;
- commit queue depth and write rate; and
- maximum inactivity and cleanup intervals.

Cleanup expires extents first, then artifacts and receipts, then checkpoints with no usable extents, then inactive
sessions. Cleanup is idempotent and resumable. When cleanup lag or storage watermarks exceed limits, admission of new
durable cache state stops while ordinary Messages inference continues.

## Configuration

One mode controls activation:

```text
messages_cache_state.mode = off | observe | route | checkpoint | retain | artifact
```

Each mode includes prior behavior. Startup rejects modes whose required router, keys, storage, or provider capabilities
are unavailable. `off`, `observe`, and `route` require no durable cache tables. `artifact` remains unavailable for a
provider until token-prefix ingestion and render-parity conformance pass.

The schema is incremental rather than a four-table prerequisite. Phase 3 adds session, checkpoint, and extent storage
only after a durable consumer exists. The artifact table and its encryption and cleanup obligations arrive only with
Phase 5. A deployment that stops after routing requires neither cache-state migrations nor cache-state cleanup workers.

Rollback selects the previous mode. Later-stage rows become inert and expire naturally; no client or schema rollback is
required.

## Implementation plan

### Phase 0: transport fidelity

- Preserve top-level and explicit `cache_control` through direct, non-streaming tool-loop, and streaming tool-loop paths.
- Preserve unknown Messages fields and open-list Claude Code headers.
- Add generated Claude Code fixtures distinct from synthetic protocol fixtures.

Exit condition: upstream request bodies are JSON-semantically equivalent except for existing documented normalization.

### Phase 1: observation

- Implement `CacheIntentParser` and cache outcome metrics.
- Normalize omitted ephemeral TTL to five minutes.
- Record no durable checkpoint state.

Exit condition: dashboards distinguish intent, routing, creation, and read outcomes with no transport regression.

### Phase 2: routing

- Implement tenant-safe scope derivation and stable cache salt.
- Add execution fingerprints and a capability-validated `CacheRoutePlanner` for llm-d or direct-provider routing.
- Keep provider handles request-local.

Exit condition: controlled multi-worker tests show a measured cache or latency improvement without unacceptable worker
skew.

### Phase 3: checkpoints

- Add Messages-owned session, checkpoint, and extent migrations and typed stores.
- Extract only proven protocol-neutral repository and lifecycle helpers shared with Responses.
- Add exact-prefix matching and bounded asynchronous commits.

Exit condition: branching, restart, database-failure, cleanup, and cross-protocol isolation tests pass, and a configured
consumer demonstrably uses the persisted state.

### Phase 4: retention

- Add provider capability for soft TTL retention hints.
- Enforce tenant/global quotas and pressure-aware admission.

Exit condition: pause-duration and cache-pressure benchmarks improve reuse without starvation or unbounded memory.

### Phase 5: portable artifacts

- Add a provider adapter accepting versioned token prefixes plus a certified suffix.
- Implement encryption, checksums, key rotation, and exact render/token parity tests.

Exit condition: token-for-token equivalence is proven for every supported request shape, and every mismatch falls back
to full rendering.

## Expected code boundaries

- HTTP Messages handlers extract validated transport context but do not own cache policy.
- Core Messages types own wire-level cache classifications.
- The executor coordinates prepare, infer, observe, and commit.
- `storage` owns Messages cache models, migrations, stores, and shared mechanical helpers.
- Provider and routing capabilities live behind adapters.
- Responses stores and rehydration remain separate typed paths.
- Existing common serialization helpers remain the policy boundary for JSON conversion in production code.

The implementation must not create a parallel Messages executor, HTTP client, database pool, or generalized JSON state
table.

## Test plan

### Protocol fidelity

- top-level automatic caching and explicit markers;
- omitted five-minute and explicit one-hour TTLs;
- zero, one, four, and unsupported extra markers;
- tools, system blocks, user and assistant content, images/documents, thinking, `tool_use`, and `tool_result`;
- mixed TTL order;
- unknown fields and content-block types; and
- direct, non-streaming tool-loop, and streaming tool-loop golden requests.

### Claude Code conformance

For every supported pinned client version, generate sanitized fixtures from a local capture server covering:

- normal and bare surfaces;
- first and resumed turns;
- full tools and disabled tools;
- default five-minute, one-hour, forced five-minute, and disabled caching;
- attribution enabled/disabled;
- dynamic system sections included/excluded across directories;
- session, agent, and parent-agent headers;
- subagent, fork, and fan-out behavior;
- model, effort, fast-mode, tool, plugin, compaction, and version invalidation; and
- breakpoint-rejection retry and streaming liveness behavior.

Raw captures may contain prompt and filesystem context and are never committed. A separate credentialed qualification
environment verifies real cache creation/read usage; the local synthetic server proves only client transport behavior.

### State and concurrency

- tenant/session HMAC determinism and key rotation;
- no cross-tenant or cross-upstream candidate lookup;
- immutable branches under concurrent requests;
- deterministic candidate ordering and deduplication;
- expiry boundaries with a controllable clock;
- queue saturation, database outage, restart, cleanup lag, and partial cleanup;
- SQLite/PostgreSQL migration and repository parity; and
- deletion racing an asynchronous commit.

### Responses semantic isolation

- a missing required previous response still fails Responses rehydration;
- the equivalent missing Messages cache state preserves the full request and proceeds;
- Messages cache IDs are not resolvable through public Responses or conversation endpoints;
- Responses IDs cannot select Messages cache checkpoints;
- Messages types do not depend on Responses item or rehydration types; and
- shared-helper fault injection preserves each protocol's distinct read/write behavior.

### Security

- malformed and high-cardinality session headers;
- forged, replayed, expired, wrong-tenant, wrong-pool, and wrong-model receipts;
- corrupt, truncated, wrong-key, and incompatible artifacts;
- quota abuse with branches and one-hour TTLs;
- logs/traces contain no raw IDs, credentials, prompts, receipts, or artifacts; and
- timing-sensitive tests confirm that tenants never share route keys or state lookup keys.

### Qualification benchmark

Compare cache off, upstream-only `cache_control`, route, checkpoint, retain, and artifact modes across:

- two-turn, 20-turn, four-breakpoint, gateway-tool-loop, fork, subagent, and branching workloads;
- pauses of 0 seconds, 30 seconds, 4 minutes, 6 minutes, 55 minutes, and 65 minutes;
- no pressure, partial capacity, eviction pressure, worker restart, and rolling churn;
- one tenant, skewed multi-tenant traffic, and adversarial session churn; and
- concurrency of 1, 16, and 128 requests.

Measure first-token and total latency, actual cache-read and creation tokens, affinity, worker skew, database load,
commit drops, memory, cleanup lag, and errors. A stage advances only when it improves latency or compute cost without
violating security, availability, skew, or capacity limits.

## Acceptance criteria

- Every supported request preserves Messages wire semantics and unknown fields.
- `/v1/messages` requires complete client context and exposes no server continuation identifier.
- Cache-state absence or failure never changes model-visible input.
- Tenant, pool, salt, and execution compatibility are enforced before reuse.
- Multiple mixed-TTL breakpoints remain ordered independent extents.
- Concurrent requests create immutable branches without a mutable latest pointer.
- Hidden tool rounds do not become public or durable continuation points.
- Actual cache outcomes are not inferred from client intent or routing.
- Durable writes occur only in `checkpoint` or later modes and only with a real state consumer.
- Responses and Messages share mechanical infrastructure without sharing rows, payloads, public IDs, rehydration, or
  failure policy.
- Every mode can roll back without changing clients or making cache state a correctness dependency.
- The qualification benchmark and security tests pass before each mode is enabled in production.

## Alternatives considered

### Forward `cache_control` only

This is required transport behavior and the correct first phase. It does not coordinate routing, retention, branches,
or portable provider state, so it cannot realize the affine-cache opportunity by itself.

### Persist complete Messages conversations

This duplicates client-owned state, increases privacy and deletion burden, and can drift from the exact request. It is
unnecessary because clients resend complete history. A future stateful Messages API would be a separate product and
wire contract.

### Reuse Responses rows and continuation IDs

The lifecycle shape is reusable; the records are not. Responses rows contain authoritative model-visible items and are
publicly addressable. Messages cache state has multiple extents, remains private, and must fail open. Direct reuse would
conflate protocol semantics and create unsafe lookup paths.

### One universal state table and handler

Runtime flags such as `authoritative`, `public`, and `fail_open` create invalid combinations that are hard to review. A
wrong flag could silently omit required Responses context or make Messages cache state mandatory. Separate typed
projections over shared low-level helpers are safer and easier to test.

### Persist checkpoint metadata before a consumer exists

Unused metadata creates write amplification, cleanup, migrations, and privacy cost without improving a request. Durable
state begins only when routing receipts, retention, or artifacts consume it.

### Store KV tensors

KV tensors are large, topology-sensitive, engine-specific, and difficult to secure or migrate. Versioned token-prefix
artifacts are the deferred portability boundary.

## Sources

- [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- [Claude Code prompt caching](https://code.claude.com/docs/en/prompt-caching)
- [Claude Code LLM gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)
- [vLLM session-centric KV-cache orchestration RFC](https://github.com/vllm-project/vllm/issues/48501)
- [vLLM agent-aware KV-cache management RFC](https://github.com/vllm-project/vllm/issues/52113)
- [llm-d-router session-centric KV lifecycle orchestration](https://github.com/llm-d/llm-d-router/issues/1979)
- [llm-d-router Session Control Protocol](https://github.com/llm-d/llm-d-router/issues/2003)
- Agentic API Responses storage and execution code under `crates/agentic-server-core/src/storage/` and
  `crates/agentic-server-core/src/executor/`
- [Affine KV continuation draft ADR](https://github.com/vllm-project/agentic-api/blob/feat/adr04-token-cache-phase2/docs/adr/ADR-04_kv_affine_continuation.md)
- [Affine KV continuation raw benchmark results](https://github.com/vllm-project/agentic-api/tree/feat/adr04-token-cache-phase2/docs/adr/results/adr-04/2026-07-13-n12)

The Claude Code observations in this document are pinned to client version 2.1.238. They are compatibility evidence,
not a promise about future marker placement or proof of real provider cache hits. Until reproducible capture fixtures
land, documented behavior remains the normative compatibility source.
