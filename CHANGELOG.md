# Changelog

All notable changes to Agentic API are documented here.

## [Unreleased]

### Added

- Added typed per-model input-modality overrides to `config.toml`
  (`[models."<served-model-id>"] input_modalities = ["text", "image"]`), validated at startup:
  unknown modality names, empty lists, duplicates, and image-only lists are rejected with the
  offending file and line (#252).

### Changed

- Modeled the Codex model catalog and the upstream model listing as typed Rust structs instead of
  untyped JSON, and reported an undecodable upstream `/v1/models` payload as `502` rather than
  serving it as an empty catalog (#252).
- `agentic run codex` and `agentic harness codex` now resolve the model and its input modalities
  from a single gateway catalog snapshot before writing an isolated Codex home, retrying a warming
  gateway and failing with an actionable error when the catalog cannot be fetched or does not list
  the selected model. A gateway behind OIDC now requires `--api-key` for `agentic harness codex`.
  `agentic_harness::prepare_codex_home` requires the resolved modalities and is no longer public
  (#252).

### Fixed

- Resolved Codex image capabilities consistently: the HTTP model catalog and both launcher modes
  now advertise the same resolved `input_modalities`, so a vision-capable model no longer has image
  content stripped client-side because an isolated catalog hardcoded `["text"]`. Existing persistent
  Codex session homes must be regenerated to pick this up (#252).

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
