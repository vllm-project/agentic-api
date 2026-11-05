---
name: storage-schema-change
description: Use when planning, implementing, or reviewing agentic-api changes to the database schema, migrations, storage models, stored JSON, or what a response or item persists.
---

# Storage Schema Change

Keep persistence portable across SQLite and PostgreSQL, keep every stored value typed, and put each piece of data in
the column whose readers need it.

## Authoritative inputs

Apply the shared [engineering preparation](../agentic-api-engineering/SKILL.md) first; reuse any completed discovery.

1. Read `AGENTS.md` and `TERMINOLOGY.md`.
2. Read the `storage/` section of `ARCHITECTURE.md`, including "Schema, migrations, and where data goes". Treat it
   as the source of truth; this skill does not replace it.
3. Read `crates/agentic-server-core/migrations/` in order, `storage/schema.rs`, and the affected `storage/models/`,
   `storage/types/`, and store files.

## Checklist

1. **Layering.** SQL lives only in `storage/models/`, as one function per operation over one row struct per table.
   Rows convert to Rust types only in `storage/types/`, through one conversion pair per column. Stores take and
   return typed values; only `executor/modes/` calls them.
2. **No backend in queries.** Model SQL is portable over `sqlx::Any`. Backend checks belong in `pool.rs`
   (connection tuning) and `schema.rs` (migration and verification), plus the existing `lock_in_tx`. Reject
   dialect functions in queries; restructure into portable queries or move the work into Rust.
3. **Migration.** A schema change is a new numbered SQL file that runs on both backends. Released migrations are
   never edited. Prefer `ALTER TABLE ADD/DROP COLUMN` to rebuilding a table, and drop a column's indexes first. New
   columns on tables that may hold rows are nullable or have a default; new integer columns are `BIGINT`.
4. **Incompatible data.** If existing rows can no longer be read, raise `MINIMUM_SUPPORTED_MIGRATION` instead of
   adding decoders for the old format, and document that the default SQLite database is recreated and other
   databases are rejected.
5. **Column or metadata.** `responses.metadata` holds only what a continuation needs and stays small, because every
   continuation decodes it and session checkpoints retain it. `items.metadata` holds storage facts about an item,
   never content, and `items.data` is the item's plain serde JSON. Data read by a different path (retrieval, for
   example), large data, data used in SQL predicates, ordering, joins, or indexes, data that needs a constraint,
   and data updated on its own each get a dedicated column. Reject a second copy of data another column holds.
6. **Typed values.** Stored JSON round-trips through a typed Rust struct or enum. No `serde_json::Value` at a
   store boundary, no hidden keys inside wire-shaped JSON, and no placeholder arguments.
7. **Everything that names the column.** Check the row struct, its SQL (including explicit column lists), the
   `types/` conversion, the supervisor verification column list and counts in `schema.rs`, the readiness probe,
   `ARCHITECTURE.md`, and the deployment docs when operators are affected.
8. **Tests.** Cover a fresh SQLite database, the ignored PostgreSQL suites (`postgres_*` tests, run with
   `TEST_POSTGRES_URL` and `--ignored`), supervisor-mode verification, and the outdated-database path when the
   minimum migration changes. Re-run the storage benchmarks when a hot read or write path changes.
