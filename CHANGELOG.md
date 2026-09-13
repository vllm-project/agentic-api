# Changelog

All notable changes to Agentic API are documented here.

## [Unreleased]

### Changed

- Forwarded `parallel_tool_calls` as the model-generation preference for typed
  Responses requests, including built-in-only and mixed tool declarations (#181).
- Added bounded, configurable parallel execution for Responses gateway rounds,
  preserving model call order and applying per-handler same-tool safety.
- Preserved MCP list-tools records in continuation history for registry lifecycle
  decisions while excluding them from model input, preventing repeated public
  list-tools emission on later turns.
- Clarified Codex tool execution roles by replacing ambiguous ownership language
  with the preferred client-executed and gateway-executed terminology.
- Refactored `web_search` into a typed provider contract and module split (`tool/web_search/{mod,args,you}`) as
  the extension seam for further providers (#291): provider responses now normalize into `WebSearchResult` /
  `WebSearchProviderMetadata` instead of forwarding raw You.com JSON, and `WebSearchProviderKind`,
  `WebSearchProviderConfig::{provider, max_concurrent_queries}` (non-exhaustive; build with
  `WebSearchProviderConfig::new`), and `WebSearchHandler::from_config` select and bound the provider. The
  model-facing tool output keeps You.com's field names and the public `web_search_call.action.sources` list is
  unchanged, but the normalization contract is now explicit: cosmetic `thumbnail_url` / `favicon_url` and unknown
  fields are dropped, keys follow the typed struct order, `null` and empty fields are omitted, and an invalid
  `freshness` fails fast with a tool config error instead of a provider round trip. `WebSearchProviderConfig` and the
  You.com credential redact the API key in `Debug` output.

### Fixed

- Rejected split-execution responses with missing, reused, or unstable tool call IDs before persistence, keeping the
  reserved response ID available for a corrected retry.
- Hardened split execution with atomic duplicate persistence, strict relayed-response validation, independent secret
  validation, bounded hydrate and persist payloads, stable error envelopes, and graceful shutdown error propagation.
- Forwarded Responses `text` generation settings through typed execution paths while preserving provider-specific
  text formats on stateless proxy requests and JSON Schema property order.
- Replaced `WebSearchActionSearch::new` and `WebSearchCall::new` with fallible
  `try_new(...)` constructors; callers now handle `WebSearchActionError` for
  empty query lists instead of risking a panic.

### Added

- Documented running Agentic API in front of NVIDIA Dynamo and recorded Dynamo cassettes for stateful and
  function-call flows.

### Testing

- Added Dynamo upstream replay tests, a generic cassette validator (`scripts/validate-cassettes.py`), and a dedicated
  CI job for them.

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
