# File Search Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Complete the missing OGX retrieval capabilities and OpenAI Files/Vector Stores operations in a manageable PR stack.

**Architecture:** Retain #34 as the foundation. Add database indexing, model-assisted retrieval, and HTTP lifecycle compatibility in three independently verified branches. Reuse the existing typed service boundaries and keep model and storage configuration deployment controlled.

**Tech Stack:** Rust 2024, Axum, SQLx, PostgreSQL/pgvector, SQLite, Tokio, reqwest.

**Spec:** `docs/superpowers/specs/2026-09-10-file-search-parity.md`

## Global Constraints

- Rust edition: 2024; minimum supported Rust version (MSRV): 1.85. The existing optional PDF feature requires Rust 1.88.
- Public request/response/tool boundaries use typed Rust, not loose JSON.
- Core owns business logic and storage; handlers own HTTP transport.
- No unsafe code; no locks held across await; bound blocking work and resources.
- Preserve local filesystem file storage and existing SQL data.
- Signed-off conventional commits; Summary and Test Plan in every stacked PR.

## Task 1: PostgreSQL vector storage and CI

**Files:** `crates/agentic-server-core/src/storage/file_search.rs`, new focused pgvector storage module, `src/tool/file_search/service.rs`, `src/tool/file_search/ranking.rs`, `src/types/file_search.rs`, server config/CLI files, `.github/workflows/rust.yml`, service tests, `docs/api/file-search.md`.

**Interfaces:** Preserve `FileSearchService::new(pool, client, config)` and existing HTTP contracts. Add typed backend/index configuration to `FileSearchConfig`; expose a bounded storage candidate query used by the shared retrieval service. Keep vector publication in the attachment transaction. Later tasks extend model configuration and service lifecycle, so keep database-specific SQL inside storage.

- [ ] Add a regression test selecting pgvector on SQLite and asserting a configuration error, plus ignored real-PostgreSQL tests for vector extension/index presence and semantic retrieval with no lexical overlap.
- [ ] Run the tests against the foundation and record the expected failure.
- [ ] Add pgvector schema initialization, typed configuration and index options, bound vector insertion/querying, filter/store isolation, deletion, dimension checks, and migration/restart handling. Test real database behavior rather than SQL string presence.
- [ ] Configure a real pgvector PostgreSQL CI service. Run PostgreSQL tests locally with an isolated task database where available; capture any environment failure precisely and resolve it through CI.
- [ ] Run `cargo test -p agentic-server-core --test file_search_service`, focused config tests, pgvector integration tests, formatting and clippy for changed targets. Commit with `git commit -s`.
- [ ] Review the full layer against the spec and fix actionable findings before stacking Task 2.

## Task 2: OGX retrieval configuration, contextual ingestion, reranking, rewriting

**Files:** `crates/agentic-server-core/src/types/file_search.rs` (or focused typed config module), `src/tool/file_search/{service,handler,ranking,ingest,embeddings}.rs`, new bounded model client module(s), server config/CLI, service/tool tests, `docs/api/file-search.md`.

**Interfaces:** Consume Task 1's bounded candidate retrieval. Keep `FileSearchService` as the shared owner for direct search and the Responses tool. Add deployment-controlled `VectorStoresConfig`; requests select supported strategies/models without supplying credentials or arbitrary endpoints. Attachments preserve original text alongside contextual embedding text.

- [ ] Read OGX `core/datatypes.py`, `providers/utils/memory/vector_store.py`, `openai_vector_store_mixin.py`, and query-rewrite routing to map actual behavior and defaults.
- [ ] Add failing behavioral tests with local model HTTP fixtures: contextual context affects embeddings but preserves source text; reranker changes order before truncation; query rewriting changes retrieval and `search_query`; malformed indexes/scores and provider failures are rejected without partial publication.
- [ ] Implement typed grouped config and validation, backward-compatible embedding settings, bounded contextual model requests, query rewriting, neural/classifier reranking, and configurable fusion/default modes. Accept documented OpenAI ranker names as selectors for the configured ranker, explaining that model scores are implementation-specific.
- [ ] Wire configuration into the server and both direct search and Responses file search. Confirm `none` bypasses the model and request parameters override configured defaults consistently.
- [ ] Run focused service/tool/config tests and changed-target clippy; commit signed off and review before Task 3.

## Task 3: Files and Vector Stores lifecycle contracts

**Files:** `crates/agentic-server-core/src/types/file_search.rs`, storage lifecycle modules/migrations, service lifecycle modules, `crates/agentic-server/src/handler/http/file_search.rs`, OpenAPI schemas, local file storage, executor citation event handling, HTTP/service/tool tests, new SDK contract test/script, compatibility documentation.

**Interfaces:** Extend existing `FileSearchService` with update/expiration/content/batch operations. Batch and expiration workers have explicit start/shutdown ownership. Keep the existing upload API usable while adding streaming publication for HTTP. Build on Tasks 1–2's atomic attachment publication and model clients.

- [ ] Fetch official Files, Vector Stores, vector store file/batch/search and annotation event schemas. Write an operation/field matrix with supported, extension, and documented operational-limit categories.
- [ ] Add failing HTTP/service contract tests for updates, nullable metadata/expiry, purpose/status filters, parsed content, batch lifecycle/cancel/restart, expiration removing search visibility, and citation annotation streaming. Test documented literal wire fields independently of local schema generation.
- [ ] Implement missing typed operations and portable durable state migrations; bound batch concurrency and recover jobs after restart. Test partial batch failure and cancellation while a model call is active.
- [ ] Implement bounded streaming filesystem upload/download for the documented file-size limit, accepted purposes and file expiration; retain atomic local publication and legacy data reads. Exercise real multipart and download bytes, disconnection cleanup and limit errors.
- [ ] Add OpenAI SDK smoke/contract verification against the local service with local model fixtures, covering upload, create, attach, search, update, parsed content, batches, cancellation, list pagination, and deletion.
- [ ] Run focused HTTP/service/tool tests, then `cargo test --workspace --all-features --locked`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`, `cargo fmt -- --check`, and `pre-commit run --all-files`.
- [ ] Review the complete stack against the spec and compatibility matrix, resolve findings, and open/update stacked PRs with actual test evidence and dependency links.

## Build resources

Use the existing shared target directory `/private/tmp/agentic-file-search-target`
with `--config profile.dev.debug=0 --config profile.test.debug=0 --config build.incremental=false`.
Disk space is limited; do not create redundant large target directories. Do not
delete user files or unrelated worktree/build artifacts to free space.
