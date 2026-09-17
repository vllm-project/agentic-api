# Contributing to vLLM Agentic API

Thank you for your interest in contributing to the vLLM Agentic API. This guide covers
everything you need to get started.

## Getting Started

1. Fork and clone the repository:
   ```bash
   git clone https://github.com/vllm-project/agentic-api.git
   cd agentic-api
   ```

2. Install prerequisites:
   - [Rust toolchain](https://rustup.rs/) (rustup).
   - [uv](https://docs.astral.sh/uv/getting-started/installation/) for docs environment and dependency setup.
   - [pre-commit](https://pre-commit.com/) for local hook execution.

3. Build and fetch dependencies:
   ```bash
   cargo build
   ```

4. Install pre-commit hooks:
   ```bash
   pre-commit install
   ```

## Development

### Running Tests

```bash
cargo test
```

### Linting

```bash
cargo clippy --all-targets -- -D warnings
```

### Formatting

```bash
cargo fmt
```

All linting and formatting checks are also run automatically via pre-commit hooks on
each commit.

## Documentation

Build docs locally:

```bash
uv venv
uv pip install -r docs/requirements.txt
uv run mkdocs serve
```

## Pull Requests

- Branch from `main`.
- Write tests for new functionality.
- Ensure all pre-commit hooks pass before pushing.
- Sign off your commits (`git commit -s`).
- Use the PR template, which includes two required sections:
  - **Summary** -- a concise description of what the PR does and why.
  - **Test Plan** -- how the changes were tested.

## Code Style

Code style is enforced by `rustfmt` and `clippy` via pre-commit. Key settings:

- Maximum line length: 120 characters.
- Rust edition: 2024.
- `unsafe` code is forbidden.

Do not worry about manually formatting code -- the pre-commit hooks will handle it.

### Rust production file sizes

Prefer production modules below 300 lines; 300–500 lines is reasonable for one
clear responsibility. The `rust-file-sizes` pre-commit hook enforces a maximum of
500 physical production lines for new files. Existing oversized files have
explicit caps in `.rust-file-sizes.json`, tracked for cleanup in
[#312](https://github.com/vllm-project/agentic-api/issues/312). Split files by
responsibility while preserving architecture boundaries, rather than moving
arbitrary lines to satisfy the check.

Run the same check used by CI:

```bash
pre-commit run rust-file-sizes --all-files
pre-commit run rust-file-size-tests --all-files
# Show the hook's success summary:
pre-commit run rust-file-sizes --all-files --verbose
```

The checker scans all Git-tracked `.rs` files on every invocation, including
staged additions. Stage new files before checking them. Full scans also catch
deleted/renamed files and policy changes. The hook's pinned Python/Rust-parser
dependencies are installed once by pre-commit and reused; a check neither builds
the Rust workspace nor accesses the network. The existing Pre-commit workflow
runs these hooks with `--all-files`.

The local hook compares baseline entries with the committed policy at `HEAD`.
CI compares against the PR base, merge-queue base, or the previous main commit,
using `RUST_FILE_SIZE_BASE`; CI fetches full history for that comparison.
This prevents an earlier commit in a PR from hiding a baseline addition or increase.
To check a whole branch locally, select an already-fetched base:

```bash
RUST_FILE_SIZE_BASE=upstream/main pre-commit run rust-file-sizes --all-files
```

The script also accepts `--base-ref`, which overrides the environment variable.
An unavailable base fails the check; fetch that revision before retrying. When
introducing the policy, initial caps may only come from production counts in
regular Rust files that already existed at the base revision. A repository's
first commit or first push has no prior allowances.

Counting rules:

- Count physical lines, including comments and blanks, with or without a final
  newline. CRLF and LF have the same count.
- Parse Rust syntax with Tree-sitter; exclude `#[cfg(test)]` items and their
  attributes, nested test modules/items/statements, fields, initializers, match
  arms, inner `#![cfg(test)]`, and built-in `#[test]`/`#[bench]` functions.
  A test-only item can appear anywhere in a file.
  `all`/`any`/`not` predicates are excluded only when they require `test`; unknown
  feature/platform configurations remain production code.
- Only remove an entire physical line from the production count when no
  non-whitespace source remains outside test-only ranges. A mixed line counts as
  both production and test. Comments/blanks outside those ranges count as
  production. Test lines have no size limit.
- Exclude files named `tests.rs` and files under a `tests`, `benches`, or `examples`
  directory. These names are reserved for dedicated non-production source.
- Macros, including `cfg_attr` and attribute macros, are not expanded. Their
  source is conservatively counted unless enclosed in a recognized test-only item.
  Rust parser errors fail the check instead of undercounting.
- Generated Rust requires an exact path in `generated`, with a nonempty string
  identifying its generator and why it is excluded. Directory globs and generated
  markers in source do not grant exclusions. Stale or conflicting entries fail.

When a baselined file shrinks, lower its cap to the reported production count in
the same commit; remove the entry at 500 lines or fewer. Delete or rename its
policy entry when deleting or renaming the file. A rename detected by Git may
retain or lower the original file's cap; copies and other new paths cannot inherit
a baseline. The checker reports the required update and never rewrites the baseline.
Outside initial setup and detected renames, new baseline entries are rejected.
Caps cannot increase relative to the selected prior revision.
A justified cohesion-based exception belongs in `exceptions`
as an exact path mapped to `{"limit": 550, "reason": "Specific rationale and review/issue reference"}`.
Keep any existing baseline cap so the exception remains explicit. Exceptions
must exceed the normal allowance and be removed when no longer needed.
Policy changes, including the initial baseline and every exception, require code
review; the checker validates policy contents, not GitHub approval state.

## Reporting Issues

Use the issue templates provided on the
[GitHub Issues](https://github.com/vllm-project/agentic-api/issues) page. Choose the
template that best matches your report (bug report, feature request, etc.).

## Code of Conduct

This project follows a Code of Conduct. Please review
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for details on expected behavior.
