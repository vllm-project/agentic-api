# Changelog

All notable changes to Agentic API are documented here.

## [0.6.0] - 2026-09-01

### Added

- Added the build-only `agentic-api` Python distribution with `serve`, `doctor`, and version commands, packaged Rust
  gateway binaries, local or remote vLLM launch modes, wheel validation, and Linux and macOS release artifacts (#201).
- Added end-to-end parallel tool calling for typed Responses requests, including bounded concurrent execution for
  gateway-executed built-in tools, batched web searches, stable output ordering, and per-call failure isolation (#214).
- Added attached Claude Code and Codex workflows with isolated model and provider configuration and recorded CLI
  coverage (#210).
- Added configurable streaming chunk timeouts for Responses and Messages streams, with a ten-minute default (#221,
  #227).
- Added deployment guides and replay coverage for NVIDIA Dynamo and llm-d Kubernetes upstreams (#207, #212).
- Added a benchmark suite comparing WebSocket, HTTP/SSE, and HTTP/JSON Agentic API flows with direct vLLM across tool
  loops, function selection, and stateful conversation workloads (#185).
- Added a repository-local pull request review skill with explicit wire-format and replay-cassette checks (#228).

### Changed

- Forwarded typed Responses reasoning configuration upstream and preserved complete streamed reasoning content,
  summaries, and opaque state (#219, #225).
- Replayed persisted plaintext reasoning safely during continuation while rejecting opaque-only state that vLLM cannot
  consume (#222).
- Preserved MCP list-tools records in item history for discovery lifecycle decisions while excluding them from model
  input, preventing repeated public discovery items on later turns (#214).
- Improved Rust and container CI caching, test setup, and path filtering to shorten release validation (#205).
- Clarified client-executed and gateway-executed tool roles in Codex integration documentation (#230).

### Fixed

- Rejected continuations that omit required function call outputs instead of proceeding with unresolved call IDs
  (#214).
- Preserved MCP and web-search public item types during mixed built-in tool rounds (#214).
- Removed connection-nominated hop-by-hop headers from proxied requests and responses as required by HTTP semantics
  (#217).
- Required a healthy packaged gateway before `agentic-api doctor --mode local` reports success (#223).
- Rebuilt workspace crates after `cargo-chef` dependency cooking so container binaries carry current source and package
  metadata (#208, #209).
- Made web-search action construction fallible so empty query lists return a typed error instead of panicking (#230).

### Testing

- Added matched OpenAI and gateway cassettes for reasoning and parallel tool calling, replay tests for Dynamo, a generic
  cassette validator, Python package and wheel test suites, and dedicated CI jobs for the new release paths.

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
