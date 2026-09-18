# Changelog

All notable changes to Agentic API are documented here.

## [Unreleased]

### Added

- Added SearXNG as a selectable backend for the gateway-owned `web_search` tool (#326, Phase 3 of #291). Select it
  with `AGENTIC_WEB_SEARCH_PROVIDER=searxng` or `[web_search] provider = "searxng"` and point
  `AGENTIC_WEB_SEARCH_BASE_URL` or `[web_search] base_url` at a self-hosted instance; the endpoint is mandatory
  (an absolute `http(s)` URL without a query or fragment, sub-path mounts allowed) and the server refuses to start
  without it. No API key is needed; `SEARXNG_API_KEY` (or the variable named by
  `api_key_env`) is sent as a `Bearer` token only when set. Web and news results come from one
  `format=json&categories=general,news` request per query, split by category. The gateway adapts the shared tool
  contract: `allowed_domains` / `blocked_domains` and the model's `include_domains` / `exclude_domains` are enforced
  client-side on a label boundary, `count` is applied client-side after filtering, `freshness` maps to `time_range`
  (date ranges are ignored), `language` is normalized to SearXNG's `xx` / `xx-YY` form, `safesearch` maps to
  `0` / `1` / `2`, and `country` plus the You.com-specific arguments are ignored. A `403` is reported as the JSON
  format being disabled, and a `429` explains SearXNG's bot-detection limiter, which rejects the gateway's
  `Accept-Encoding`-free requests unless its address is on `pass_ip`; neither is retried. Each SearXNG `metadata[]`
  entry carries `"provider": "searxng"`. Concurrency inherits `max_concurrent_gateway_calls`.
- Added typed per-model input-modality overrides to `config.toml`
  (`[models."<served-model-id>"] input_modalities = ["text", "image"]`), validated at startup:
  unknown modality names, empty lists, duplicates, and image-only lists are rejected with the
  offending file and line (#252).
- Added Brave Search as a selectable backend for the gateway-executed built-in `web_search` tool (#294, Phase 2 of #291).
  Select it with `AGENTIC_WEB_SEARCH_PROVIDER=brave` or `[web_search] provider = "brave"` and supply `BRAVE_API_KEY`;
  the endpoint defaults to `https://api.search.brave.com` and can be overridden with `AGENTIC_WEB_SEARCH_BASE_URL`
  or `[web_search] base_url`. Web and news results come from one request per query. The gateway adapts the shared
  tool contract: `allowed_domains` / `blocked_domains` and the model's `include_domains` / `exclude_domains` are
  enforced client-side on a label boundary, `count` is clamped to Brave's maximum of 20, `freshness` is rendered in
  Brave syntax, `language` maps to `search_lang`, and the You.com-specific `livecrawl`, `livecrawl_formats`,
  `crawl_timeout`, and `boost_domains` arguments are ignored. Rejected credentials and HTTP 429 responses fail the
  `web_search_call` without an automatic retry, naming the key variable or the upstream `Retry-After` value and never
  echoing the secret. Each Brave `metadata[]` entry carries `"provider": "brave"`.
- Added `[web_search] max_concurrent_queries` and `AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES` to cap concurrent
  provider requests inside one batched search. Brave defaults to `1` for its free-plan rate limit; You.com keeps
  inheriting `max_concurrent_gateway_calls`. The effective ceiling is the smallest of the gateway limit, this
  override, and the provider's own ceiling.

### Changed

- `WebSearchProviderKind` gains a `Searxng` variant (`"searxng"`) with no default endpoint, `SEARXNG_API_KEY` as
  its conventional key variable, and no provider concurrency ceiling. `WebSearchProviderKind::ALL` grows from
  `[Self; 2]` to `[Self; 3]` (it enumerates every selectable provider and will grow again with each one); iterating
  it is unaffected, but code that destructured or annotated the fixed length must be updated.
  `agentic_core::tool::SEARXNG_BASE_URL_HINT` and `validate_searxng_base_url` carry the operator-facing rules for
  the mandatory endpoint (absolute `http(s)` URL with a host and no query or fragment). The shared
  `null_as_default` and `read_response_limited` helpers moved from `web_search/mod.rs` to `web_search/provider.rs`
  (crate-private, re-exported unchanged).
- Modeled the Codex model catalog and the upstream model listing as typed Rust structs instead of
  untyped JSON, and reported an undecodable upstream `/v1/models` payload as `502` rather than
  serving it as an empty catalog (#252).
- `agentic run codex` and `agentic harness codex` now resolve the model and its input modalities
  from a single gateway catalog snapshot before writing an isolated Codex home, retrying a warming
  gateway and failing with an actionable error when the catalog cannot be fetched or does not list
  the selected model. A gateway behind OIDC now requires `--api-key` for `agentic harness codex`.
  `agentic_harness::prepare_codex_home` requires the resolved modalities and is no longer public
  (#252).
- `WebSearchProviderConfig` is now `#[non_exhaustive]` and gains `provider` and `max_concurrent_queries` fields;
  construct it with `WebSearchProviderConfig::new(api_key, base_url)` plus the `with_provider` and
  `with_max_concurrent_queries` builders. Downstream crates that built it with a struct literal must switch to the
  constructor; field reads and `Default` are unchanged. `WebSearchProviderKind` gains a `Brave` variant, `FromStr`
  (case-insensitive), `default_base_url`, `default_max_concurrent_queries`, and `config_name`;
  `WebSearchHandler::from_config` builds the handler for the selected provider and `GatewayExecutors::from_config`
  uses it. With `provider` unset, You.com behavior, configuration, and model-facing output are unchanged; a generated
  `config.toml` now records `provider = "you"` and leaves `api_key_env` unset so provider switches select the matching
  default credential variable.

### Fixed

- Rejected message content the typed Responses executor cannot convey — unmodeled part types and empty part arrays,
  alongside the existing `input_file` rejection — with a `400` naming the offending part, instead of forwarding a
  synthetic `{"type": "unknown"}` part or silently dropping it. Modeled parts keep their unmodeled extension fields
  through the typed path, so a message is never mutated in transit, never means something different on the typed
  path than on the raw `store: false` path, and is never persisted with content the client did not send (#253).
- Counted an image referenced by `file_id` as retained context during compaction, matching inline images (#253).
- Followed MCP `tools/list` pagination to discover tools beyond the first page, including opaque empty cursors;
  reject repeated cursors and bounded-pagination failures instead of exposing partial discovery (#311).
- Accounted for unrestricted output role/type/status strings, empty web-search query entries, pending or late-bound
  item identities, and terminal error details in response limits. Kept reasoning-part and shell-command completion
  accounting linear for sequential multipart streams (#304).
- Charged the Responses retained-byte budget for logical output (text, arguments, annotations, nested JSON, and one
  structural charge per retained entry, including empty JSON values) instead of raw upstream SSE line bytes, so fine-grained
  chunking, coarse chunking, and non-streaming JSON consume identical budget, and empty or done-only parts are charged as they arrive (#288, #304).
- Replaced the fixed 1 MiB Responses WebSocket event ceiling with the configured `max_stream_event_bytes`; the
  executor now validates the terminal `response.completed` event against the WebSocket transport limit, including
  `stream_id` routing metadata, before persisting the response or publishing a session checkpoint (#304).
- Added independent, validated `[responses]` limits for upstream JSON bodies, upstream SSE lines, retained output, and
  client stream events, with a typed `ResourceLimitExceeded` error that maps upstream overflows to HTTP 502 (#288).
- Resolved Codex image capabilities consistently: the HTTP model catalog and both launcher modes
  now advertise the same resolved `input_modalities`, so a vision-capable model no longer has image
  content stripped client-side because an isolated catalog hardcoded `["text"]`. Existing persistent
  Codex session homes must be regenerated to pick this up (#252).

### Testing

- Extended the pinned Codex 0.149.1 smoke with actual PNG attachments through both launcher modes, exact upstream
  image-byte assertions, and a text-only negative control. The smoke replays the committed vision recording without
  live API credentials (#261).

## [0.7.0] - 2026-09-14

### Added

- Added client-executed shell tools with typed `shell_call` and `shell_call_output` items, incremental command
  streaming, explicit tool selection, and stored-history continuation (#264).
- Added a configurable serialized request size limit for HTTP bodies and WebSocket messages through
  `--max-request-body-size-bytes`, `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES`, or `[server] max_request_body_size_bytes`,
  retaining the 10 MiB default (#260).
- Added the Agentic API website, versioned documentation navigation, contributor profiles, and automatic website
  deployment after crate releases (#272, #285, #287).

### Changed

- Refactored `web_search` into a typed provider contract and module split (`tool/web_search/{mod,args,you}`) as
  the extension seam for further providers (#291): provider responses now normalize into `WebSearchResult` /
  `WebSearchProviderMetadata` instead of forwarding raw You.com JSON, and the provider trait exposes a
  `max_concurrent_requests` ceiling that bounds query fan-out. The model-facing tool output keeps You.com's field
  names and the public `web_search_call.action.sources` list is unchanged, but the normalization contract is now
  explicit: cosmetic `thumbnail_url` / `original_thumbnail_url` / `favicon_url` and unknown fields are dropped, keys
  follow the typed struct order, `null` and empty fields are omitted, and each query has a metadata object even
  when the provider omits metadata or returns `null`. Its `query` falls back to the submitted query; absent
  `search_uuid` and `latency` remain omitted. This intentionally changes the model-facing output from
  `metadata: [null]` to `metadata: [{"query": "..."}]` in that case. An invalid `freshness` fails fast with a
  tool config error instead of a provider round trip. The You.com response body and aggregate tool-output size
  limits are unchanged. `WebSearchProviderConfig` keeps its existing public shape, gains a `new` constructor, and
  redacts the API key in `Debug` output; provider selection and per-provider concurrency configuration are
  deferred to the first additional provider.


- Unified Responses JSON and SSE processing under `AgentPipeline`, sharing synchronous ingestion, typed output-item
  assembly, tool-call translation, lifecycle validation, and ordered client delivery (#274).
- Introduced `MessagesRequestContext` for the Messages tool loop, preserving unmodeled upstream fields while
  centralizing request mutation and web-search budgets (#249).
- Changed Rust integration APIs: Messages loops now accept `MessagesRequestContext`, the public `function_sse` module
  was removed, and gateway configuration uses `GatewayOptions`. Downstream crate consumers must adapt affected
  integrations (#249, #274, #260).

### Fixed

- Made stream delivery cancellation-safe, committing lifecycle and sequence state only after successful delivery and
  bounding deferred events by count and bytes (#302).
- Finished Messages inference rounds at `message_stop`, and allowed Messages to answer after forced tool use
  (#290, #296).
- Preserved incomplete upstream terminal status when an SSE completion event carries an incomplete response (#277).
- Honored Responses WebSocket storage settings with bounded connection-local sessions (#257).
- Applied the configured streaming chunk timeout to stalled upstream error-body reads in Responses streaming and
  Messages tool-loop requests (#286).

### Testing

- Added opt-in Python wheel publishing through PyPI Trusted Publishing, using the declared Cargo workspace version,
  and rejected duplicate PyPI and crates.io releases before building (#301).
- Expanded shell-tool replay and continuation coverage, shared-ingestion lifecycle checks, WebSocket session and
  storage tests, request-size boundary tests, and stalled upstream error-body regressions.

## [0.6.0] - 2026-09-09

### Added

- Added the build-only `agentic-api` Python distribution with `serve`, `doctor`, and version commands, packaged Rust
  gateway binaries, local or remote vLLM launch modes, wheel validation, and Linux and macOS release artifacts (#201).
- Added end-to-end parallel tool calling for typed Responses requests, including forwarding the model-generation
  preference, bounded concurrent execution for gateway-executed built-in tools, batched web searches, stable output
  ordering, and per-call failure isolation (#181, #214).
- Added attached Claude Code and Codex workflows with isolated model and provider configuration and recorded CLI
  coverage (#210).
- Added configurable streaming chunk timeouts for Responses and Messages streams, with a ten-minute default (#221,
  #227).
- Added an `agentic-llm-d` split-execution backend with authenticated hydrate and persist endpoints for the llm-d
  coordinator (#216).
- Added deployment guides and replay coverage for NVIDIA Dynamo and llm-d Kubernetes upstreams, including persistent
  PostgreSQL storage in the kind guide (#184, #207, #212).
- Added a benchmark suite comparing WebSocket, HTTP/SSE, and HTTP/JSON Agentic API flows with direct vLLM across tool
  loops, function selection, and stateful conversation workloads (#185).
- Added a repository-local pull request review skill with explicit wire-format and replay-cassette checks (#228).
- Added client tool search support with typed tool discovery, deferred tool materialization, stateful continuation, and
  recorded streaming, non-streaming, and WebSocket coverage (#186).
- Added concurrent Responses WebSocket multiplexing with per-request `stream_id` routing, FIFO ordering within each
  stream, and bounded concurrency across streams (#240).
- Added compile-time OpenAPI 3.1 schema generation and checked-in schema validation for the HTTP API (#229).
- Added pinned SGLang conformance recordings, replay coverage, and launch and recording guidance (#267).

### Changed

- Forwarded typed Responses reasoning configuration upstream and preserved complete streamed reasoning content,
  summaries, and opaque state (#219, #225).
- Replayed persisted plaintext reasoning safely during continuation while rejecting opaque-only state that vLLM cannot
  consume (#222).
- Preserved MCP list-tools records in item history for discovery lifecycle decisions while excluding them from model
  input, preventing repeated public discovery items on later turns (#214).
- Improved Rust and container CI caching, test setup, and path filtering to shorten release validation (#205).
- Clarified client-executed and gateway-executed tool roles in Codex integration documentation (#230).
- Documented executor streaming ownership and validation boundaries, with a repository review skill for enforcing the
  architecture (#246).
- Updated the execution architecture documentation to match the current scheduler and llm-d backend (#270).
- Preserved the typed `ignore_eos` extension when forwarding Responses requests to vLLM (#268).

### Fixed

- Rejected continuations that omit required function call outputs instead of proceeding with unresolved call IDs
  (#214).
- Preserved MCP and web-search public item types during mixed built-in tool rounds (#214).
- Removed connection-nominated hop-by-hop headers from proxied requests and responses as required by HTTP semantics
  (#217).
- Required a healthy packaged gateway before `agentic-api doctor --mode local` reports success (#223).
- Rebuilt workspace crates after `cargo-chef` dependency cooking so container binaries carry current source and package
  metadata (#208, #209).
- Hardened split execution with atomic duplicate persistence, strict relayed-response validation, independent secret
  validation, bounded hydrate and persist payloads, stable error envelopes, and graceful shutdown error propagation
  (#235).
- Rejected relayed responses with missing, reused, or unstable tool call IDs before persistence, while preserving the
  reserved response ID for corrected retries (#237).
- Aligned relayed SSE validation with provider-compatible event shapes while continuing to reject inconsistent
  lifecycles and terminal items (#236).
- Forwarded Responses `text` generation settings through typed execution paths while preserving provider-specific
  stateless proxy payloads and JSON Schema property order (#231, #234).
- Bounded WebSocket queues, response data, gateway tool results, and MCP discovery and transport payloads so concurrent
  response streams cannot grow memory without limit (#240).
- Enforced CLI readiness deadlines across probes and retry sleeps, including stalled and late-success cases (#265).
- Cleaned up model subprocesses when startup is interrupted or fails during readiness and database initialization
  (#266).
- Preserved upstream error headers and content types on non-streaming Responses errors (#250, #262).
- Accepted upstream SSE `data:` fields with or without an optional separating space (#269).
- Rejected unsupported message file content on typed Responses paths instead of silently dropping it (#258).
- Excluded image bytes from compaction token estimates while continuing to count surrounding text (#255, #259).
- Treated negative upstream `sequence_number` sentinels as unspecified while preserving otherwise valid streaming
  events (#267).
- Made web-search action construction fallible so empty query lists return a typed error instead of panicking (#230).

### Testing

- Added matched OpenAI and gateway cassettes for reasoning and parallel tool calling, replay tests for Dynamo, a generic
  cassette validator, Python package and wheel test suites, and dedicated CI jobs for the new release paths.
- Strengthened multi-round cassette assertions for public stream ordering and stabilized Python readiness retry coverage
  across supported interpreter versions (#242, #247).
- Added regression coverage for structured `input_text` items that omit an explicit message type (#150, #248).

## [0.5.0] - 2026-08-25

### Changed

- Preserved Claude Code Messages transport fidelity across the gateway.
- Updated You.com web search integration to use GET query parameters.
- Aligned deployment and harness documentation with the 0.4.0 release.

### Testing

- Fixed web search test hangs in CI.

## [0.4.0] - 2026-08-23

### Added

- Added the Agentic API harness CLI for running Codex and Claude Code against Agentic API.
- Added home-based configuration and typed tool settings for standalone deployments.
- Added support for Codex CLI remote compaction V2.
- Added Kubernetes deployment guidance and architecture documentation.

### Changed

- Improved handling of Codex and Claude harness upstream configuration and compatible reasoning effort values.
- Preserved unsupported parallel tool calls through serialized upstream requests.
- Hardened MCP configuration and startup behavior.
- Improved Kubernetes health and readiness behavior for read-only container roots.

### Testing

- Added native Codex and Claude harness coverage and expanded compatibility tests.

## [0.3.0]

Initial documented release.
