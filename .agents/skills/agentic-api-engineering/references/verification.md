# Verification and completion

## Choose coverage from the changed behavior

Read the affected tests and the current `.github/workflows/rust.yml`, applicable Python workflows, and
`.pre-commit-config.yaml`. Start with a regression that demonstrates the problem at the owning boundary. Add negative
cases and subsequent rounds when those are necessary to distinguish a real fix from a first-request workaround.
Avoid implementation-mirroring tests or new scaffolding for a trivial reversible edit.

| Change | Relevant checks |
| --- | --- |
| Types/serde/schema | Accepted and malformed known variants, shorthand, absent/null/default fields, round trips, structured content, generated OpenAPI agreement, programmatic validation |
| Tools/normalization | Declaration validation, typed and proxy routes as applicable, actual upstream payload, public results, replay, next tool round, errors/limits |
| Storage/rehydration | Conversations and previous-response continuations, transient sessions, occurrence/public identity, ordering, rollback, tenant isolation, SQLite and configured PostgreSQL tests |
| Compaction/history | Explicit/automatic/trigger paths, open async calls, late/repeated outputs, public custom/shell forms, suffix retention, replay and storage after compaction |
| Streaming | JSON/SSE/WebSocket parity where supported, event lifecycle, malformed or out-of-order events, cancellation/disconnect, budgets/backpressure and terminal errors |
| Recorder scripts | CLI/help and shell syntax, scenario options and existing recorder tests, real captures when parity requires them, and Rust replay assertions |
| Python packaging/CLI | Existing `tests/python/` and CLI E2E commands from `AGENTS.md`, when affected |
| Skills/docs only | Skill frontmatter/metadata, local reference paths, command examples/options, documentation checks; do not claim a Rust test run or invoke live recordings unnecessarily |

For implementation changes in shared Rust types/execution/storage, run the focused regression and full workspace
suite, plus the applicable feature/backend jobs from CI. For read-only reviews, inspect trustworthy results for the
exact head and run the checks needed to resolve uncertainty; disclose any unverified coverage. Do not equate another
revision's results with this one. For narrow docs/tooling changes, run the relevant checks and explain the scope.

Default Rust verification, from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

Run the repository's production file-size hook for Rust changes, and its checker tests when changing the checker,
policy, or hook configuration. The current hook pins the parser dependencies; use those versions rather than a new
checker implementation:

```bash
uv run --no-project --with tree-sitter==0.25.2 --with tree-sitter-rust==0.24.2 \
  python scripts/check_rust_file_sizes.py
```

Read the current hook before copying dependency versions. The checker discovers files through the Git index; account
for new/renamed files without accidentally staging unrelated user work. Do not raise a baseline, add an exemption, or
split a cohesive function arbitrarily just to silence the check. Run full pre-commit before proposing a PR as required
by `AGENTS.md`; remember that some hooks modify files, then recheck the resulting diff.

## Keep execution and evidence accurate

- Run feature-specific and PostgreSQL jobs when affected and their prerequisites are available. Do not blindly use
  `--all-features`: embedded interpreter jobs have build/runtime requirements documented in CI. Report unavailable
  databases, services, credentials, runtimes, or network access and exactly which coverage that prevents.
- Use existing cassette-required switches when a test would otherwise silently return early without fixtures. An
  ignored test or an empty/filtered test group is not evidence that the behavior passed.
- Avoid overlapping Cargo builds/test runs in the same target directory when revisions or feature sets change.
  Rebuilding while an earlier run is executing can invalidate its binaries. Finish or stop the outdated run before
  rebuilding; use a separate target directory only when actual isolation is needed.
- Complete the final edit before the final verification. If code changes during a run, identify which results remain
  applicable and rerun the affected checks. Do not report an interrupted run as successful or aggregate duplicates
  across filtered runs into a misleading count of distinct tests.
- On failure, distinguish a product regression from missing prerequisites, a build/runner failure, or an existing
  unrelated problem. Retain the failing command/result, fix what is in scope, and report the rerun. Do not hide the
  failure with a lint allowance, ignored test, weaker assertion, deleted cassette, or changed expected output.
- Once required checks pass, stop repeating them unless a new change, failure, or unresolved concern justifies it.

## Final self-review and report

Inspect the complete final diff and all changed callers. Check that the fix addresses the demonstrated issue, keeps
one owner for each rule, removes replaced code, preserves public/model/storage distinctions, and stays within scope.
Use `git diff --check`; inspect new files as well as tracked diffs. Preserve unrelated working-tree changes.

Verify that `ARCHITECTURE.md`, affected README files, and relevant design documents match the final implementation.
Check changed examples, links, and references to renamed or removed APIs. Run the applicable documentation checks;
report the documentation updated, or explain why the change requires none.

Report the behavior changed, relevant design choice, test commands/results, failures and reruns, ignored/skipped
coverage with reasons, and the provider documentation/cassette evidence or its limitations. State whether the full
workspace suite actually ran. For reviews, give actionable findings with exact file/line, trigger, impact, and fix
location; separate pre-existing limitations and optional improvements. For pure refactors, explain why no new provider
recording is required. Do not commit, push, or publish comments without the user's authorization.
