# Changelog

All notable changes to Agentic API are documented here.

## [0.7.0] - 2026-09-11

### Added

- Added client-executed shell tools with typed `shell_call` and `shell_call_output` items, incremental command
  streaming, explicit tool selection, and stored-history continuation (#264).
- Added a configurable serialized request size limit for HTTP bodies and WebSocket messages through
  `--max-request-body-size-bytes`, `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES`, or `[server] max_request_body_size_bytes`,
  retaining the 10 MiB default (#260).
- Added the Agentic API website, versioned documentation navigation, contributor profiles, and automatic website
  deployment after crate releases (#272, #285, #287).

### Changed

- Unified Responses JSON and SSE processing under `AgentPipeline`, sharing synchronous ingestion, typed output-item
  assembly, tool-call translation, lifecycle validation, and ordered client delivery (#274).
- Introduced `MessagesRequestContext` for the Messages tool loop, preserving unmodeled upstream fields while
  centralizing request mutation and web-search budgets (#249).
- Changed Rust integration APIs: Messages loops now accept `MessagesRequestContext`, the public `function_sse` module
  was removed, and gateway configuration uses `GatewayOptions`. Downstream crate consumers must adapt affected
  integrations (#249, #274, #260).

### Fixed

- Preserved incomplete upstream terminal status when an SSE completion event carries an incomplete response (#277).
- Honored Responses WebSocket storage settings with bounded connection-local sessions (#257).
- Applied the configured streaming chunk timeout to stalled upstream error-body reads in Responses streaming and
  Messages tool-loop requests (#286).

### Testing

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
