# File Search Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Complete the missing OGX retrieval capabilities and OpenAI Files/Vector Stores operations in a manageable PR stack.

**Architecture:** Retain #34 as the foundation. Add database indexing, model-assisted retrieval, and HTTP lifecycle compatibility in independently verified branches. Reuse the existing typed service boundaries and keep model and storage configuration deployment controlled.

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

## Task 3: Streaming Files API and file expiration

**Branch:** `codex/files-api`, based on the retrieval branch.

**Files:** core file wire types, `storage/local_files.rs`, file metadata operations, Files service methods, server file HTTP handlers, Files HTTP/service tests, `docs/api/file-search.md`.

**Interfaces:** Keep existing byte-vector upload/read convenience methods usable. Add streaming publication/download primitives for HTTP, with transport concerns in handlers and bounded storage streams in core. Extend file records compatibly with expiration. Later batch/store workers use the same expiration visibility and cleanup methods.

- [ ] Use official Files schemas and the independently written SDK cases in the SDD workspace. Add red tests for accepted purposes, multipart expiration, purpose filtering, list limit/default, delete object literal, and upload/download above the previous size limit.
- [ ] Implement bounded streaming filesystem upload/download for the documented 512 MB limit; bound all multipart fields, preserve atomic publication, safe storage IDs, cancellation cleanup, and legacy inline reads. Keep explicit format-specific ingestion limits.
- [ ] Implement nullable/optional expiration timestamps and batch-purpose default expiration. Expired files must disappear from read/list/search and their attachments must be removed; expose cleanup for the lifecycle worker without deleting unrelated uploads.
- [ ] Test real multipart and download bytes, disconnect/error cleanup, expiration visibility/deletion, legacy reads, and boundary errors on SQLite and PostgreSQL. Run covering service/HTTP tests, changed-target clippy, formatting, and pre-commit; commit signed off and review.

## Task 4: Vector Stores contracts and expiration

**Branch:** `codex/vector-store-lifecycle`, based on the Files API branch.

**Files:** store/file update and content types, storage/service lifecycle modules and migration, server Vector Stores/file handlers, OpenAPI schemas, HTTP/service/model tests, compatibility documentation.

**Interfaces:** Extend FileSearchService with typed store updates, expiration policies/status, attachment attributes/status filtering, and parsed original content. Expose bounded explicit expiration cleanup for the next layer's runtime. Preserve existing atomic ingestion and immediate visibility rules.

- [ ] Add red tests for nullable store/attribute updates, expiry, filtered pagination and parsed original content, using official schemas and strict SDK cases.
- [ ] Implement typed omitted/null/value updates, store expiration/activity, attachment attribute updates and content pages. Expired stores stop serving search data while preserving uploaded files and expired metadata.
- [ ] Serialize policy updates, expiration cleanup and publication on SQLite/PostgreSQL; recheck visibility after slow model calls and prevent expired stores from reviving.
- [ ] Test expiration during model calls, policy update versus cleanup, original content without duplicated overlap/context, filtered keyset pagination and independent upload preservation on SQLite and real PostgreSQL.
- [ ] Run covering service/HTTP/SDK tests, changed-target clippy, formatting and pre-commit; commit signed off and review.

## Task 5: Durable Vector Store file batches and worker runtime

**Branch:** `codex/vector-store-batches`, based on the Vector Stores lifecycle branch.

**Files:** typed batch/job contracts, portable durable storage migration/modules, service publication integration and worker runtime, server batch handlers/startup/shutdown, OpenAPI schemas, lifecycle/runtime/HTTP/model tests, documentation.

**Interfaces:** Consume Tasks3–4's file/store visibility and cleanup. Reuse the existing prepared attachment and atomic publication path. Runtime ownership stays separate from the cloneable request service.

- [ ] Add red batch/SDK tests for create/retrieve/cancel/list-files, per-file options/counts, in-progress visibility, partial failure and pagination.
- [ ] Implement durable membership/jobs, bounded admission, claims/leases/generation fencing and restart recovery. Prevent stale publication after cancellation, claim loss, detach/re-attach, deletion or expiry across multiple server instances.
- [ ] Wire explicit worker start/shutdown into every server exit path. Shutdown joins owned work and preserves resumability; it does not cancel the API batch. Run prior layers' bounded expiration and durable blob cleanup from this runtime.
- [ ] Test competing runtimes, lease expiry, blocked-model cancellation, restart, parent mutation, counts/pagination, cleanup replay and shutdown on SQLite and real PostgreSQL.
- [ ] Run covering service/HTTP/SDK tests, changed-target clippy, formatting and pre-commit; commit signed off and review.

## Task 6: Responses citation events and SDK conformance verification

**Branch:** `codex/file-search-openai-api`, based on the durable batches branch.

**Files:** typed Responses annotation events, executor citation event handling, HTTP/tool tests, SDK contract test/script and CI invocation, OpenAPI schemas, compatibility documentation.

**Interfaces:** Preserve current Responses output indexes, continuation, lifecycle events, citation validation and final output. Emit typed annotation events consistently through the same accumulator sequencing. SDK tests exercise the server with local fixtures and strict response validation.

- [ ] Add failing stream tests for `response.output_text.annotation.added` with correct item/content/annotation indexes and sequence numbers, consistent with final output, including split deltas and no duplicate annotations.
- [ ] Implement annotation streaming and any remaining documented wire mismatches discovered by the independently derived HTTP/SDK contracts. Keep final citations grounded to retrieved files.
- [ ] Adopt the strict OpenAI SDK harness into the repository and CI with a pinned SDK version. Cover upload, create, attach, search, update, parsed content, per-file batches, cancellation, list pagination, and deletion; all model calls use local fixtures.
- [ ] Publish an explicit compatibility matrix distinguishing implemented OpenAI contracts, OGX extensions, and operational ingestion limits. Scope conformance to Files/Vector Stores/file-search behavior; do not claim proprietary retrieval-model parity.
- [ ] Run focused HTTP/service/tool/SDK tests, then `cargo test --workspace --all-features --locked`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`, `cargo fmt -- --check`, and `pre-commit run --all-files`. Verify all ignored PostgreSQL integration tests with the real pgvector runtime.
- [ ] Review the complete stack against the spec and compatibility matrix, resolve findings, and update all stacked PRs with actual test evidence and dependency links.

## Build resources

Use the existing shared target directory `/private/tmp/agentic-file-search-target`
with `--config profile.dev.debug=0 --config profile.test.debug=0 --config build.incremental=false`.
Disk space is limited; do not create redundant large target directories. Do not
delete user files or unrelated worktree/build artifacts to free space.
