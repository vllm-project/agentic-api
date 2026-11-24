# In-tree file search implementation plan

> Execute the independent core service and tool integration tasks with the parallel
> agent workflow; integrate HTTP/configuration locally and review the whole branch.

**Goal:** Reproduce OGX's central file ingestion and search flow inside Agentic API.

**Architecture:** Typed contracts in core types, durable SQL storage, a shared
ingestion/retrieval service, and the existing gateway tool interfaces. HTTP handlers
call core services through the execution context.

**Tech stack:** Rust 2024, Tokio, SQLx SQLite/PostgreSQL, Reqwest, Axum.

**Spec:** `docs/design/in-tree-file-search.md`.

## Global constraints

- Typed public APIs; no new loose JSON payload boundaries.
- No OGX service or Python runtime dependency.
- Preserve current uncommitted work outside this worktree.
- Sign off commits, use conventional prefixes, target main, and pass pre-commit.
- Preserve source errors; use bounded async I/O and offload CPU work.

## Task 1: Durable ingestion and retrieval

- [x] Create types, SQL migration/storage, and the service specified in the design.
- [x] First add tests for ingestion/search persistence and invalid search inputs.
- [x] Run those tests to observe failure, then implement the service.
- [x] Add and pass focused tests for PDF/text extraction, Unicode chunking,
  embedding protocol validation, filters, ranking, limits, deletion, and rollback.
- [x] Report exact public interfaces and validation results for integration.

Owns `types/file_search.rs`, `storage/file_search.rs`, the additive migration,
`tool/file_search/{service,ingest,embeddings,ranking}.rs`, and service tests.

## Task 2: Responses built-in tool

- [x] Add failing file-search normalization and Responses output/lifecycle tests.
- [x] Implement `FileSearchHandler`, typed declaration validation and function
  normalization, a registered executable tool slot, public call items and events.
- [x] Execute searches through the shared service interface in the design.
- [x] Preserve private model-facing call outputs for continuation; expose search
  results only when requested, and preserve file IDs and filenames for citations.
- [x] Pass focused tool and lifecycle tests and report integration touchpoints.

Owns `tool/file_search/handler.rs`, tool registry/normalization/executors,
file-search output types, and required event/executor projection changes.

## Task 3: HTTP, configuration, and whole-flow verification

- [x] Add failing tests for upload, create store, attach, search, retrieve, delete.
- [x] Add typed HTTP handlers and routes; initialize the shared service through
  `ExecutionContext` and expose deployment embedding configuration.
- [x] Verify full semantic search with an embedding HTTP fixture, keyword and
  hybrid modes, errors, and Responses blocking/streaming continuation.
- [x] Add usage documentation and OpenAPI definitions.
- [x] Run workspace tests, all-feature/OpenAPI checks, Clippy, rustfmt, pre-commit.
- [x] Review the full diff, fix findings, and prepare a signed-off commit and PR.

## Progress

- Branch `feat/in-tree-file-search` created at `c80f5bc` in an isolated worktree.
- Baseline core library: 506 passed, 26 local socket permission failures, 3 ignored.
- No implementation changes made before the first failing feature tests.

- Completed the shared service, 14 HTTP operations, embedding configuration, and
  Responses tool integration, including stored continuation and citations.
- Added optional bounded PDF ingestion; default builds retain text ingestion.
  The PDF feature requires Rust 1.88 or newer.
- Initial final workspace suite and all-target/all-feature Clippy passed.
- Independent review found four issues (tokenization latency, backward pagination,
  capacity consistency, built-in tool selectors). Regression tests cover all four,
  and the independent re-review found no remaining important issue.
- PostgreSQL validation is configured in CI. Local PostgreSQL execution was
  unavailable because the Docker VM failed to create a temporary filesystem mount.
- Rebased onto `ab0437a` (including shell tool wire types); independent review
  confirmed both shell and file-search execution/streaming paths are preserved.
- Final verification: workspace tests with all features, default text-only service
  tests, all-target/all-feature Clippy with warnings denied, formatting, OpenAPI
  tests, and all pre-commit hooks passed.
- Prepared a signed-off feature commit and updated PR description for PR #34.

## Local Files API storage follow-up

The Files and vector stores HTTP operations remain part of this branch. New file
uploads will place bytes in a configurable local directory, while SQLite or
PostgreSQL retains metadata, attachments, chunks, and embeddings. Existing inline
file bytes from the earlier draft remain readable. Binary uploads are independent
of vector-store extraction support.

- [x] Confirm failing config and binary-upload HTTP regressions.
- [x] Add `[files] storage_dir` and `AGENTIC_FILES_STORAGE_DIR` configuration.
- [x] Add HTTP lifecycle coverage for metadata, download, detachment, and deletion.
- [x] Update deployment documentation for persistent and shared file directories.
- [x] Verify atomic local publication, cancellation cleanup, bounded reads,
  filesystem errors, and service restart behavior.
- [x] Complete review, workspace checks, and update both published branches and PR.

Validation completed for the filesystem follow-up: the full workspace suite with
all features, default/PDF service tests, HTTP and configuration tests, all-target
all-feature Clippy with warnings denied, formatting, and pre-commit checks pass.
Independent review identified and verified fixes for cancelled-read capacity and
new-directory durability. The SQL migration remains unchanged.
