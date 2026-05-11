# AGENTS.md

Instructions for AI coding agents working on this repository.

## Project Overview

This repository is Rust-first under the `vllm-project` GitHub organization.

- **Rust** -- primary and active implementation language at the repo root.
- **Docs** -- MkDocs documentation in `docs/`.
- **Python gateway code has been removed** as part of the migration plan.

## Project Structure

```
.
├── src/              # Rust source code
├── Cargo.toml        # Rust package manifest
├── rustfmt.toml      # Rust formatter config
├── clippy.toml       # Clippy linter config
└── docs/             # Documentation (MkDocs)
```

## Setup

Build the project:

```bash
cargo build
```

## Testing

```bash
cargo test
```

## Linting and Formatting

```bash
cargo clippy --all-targets -- -D warnings   # lint
cargo fmt                                     # format
cargo fmt -- --check                          # check formatting only
```

To run all pre-commit hooks manually:

```bash
pre-commit run --all-files
```

## Documentation

Install docs dependencies and run docs locally:

```bash
uv venv
uv pip install -r docs/requirements.txt
uv run mkdocs serve
```

## Code Style

- Rust edition: 2024.
- Maximum line length: 120 characters (configured in `rustfmt.toml`).
- `unsafe` code is forbidden (`unsafe_code = "forbid"` in `Cargo.toml`).
- Clippy `all` lints are denied; `pedantic` lints are warnings.
- Minimum supported Rust version (MSRV): 1.85.

## Commits

- Always sign off commits with the `-s` flag (`git commit -s`).
- Use conventional commit prefixes:
  - `feat:` -- new feature
  - `fix:` -- bug fix
  - `ci:` -- CI/CD changes
  - `chore:` -- maintenance tasks (deps, config)
  - `docs:` -- documentation only

## Pull Requests

- Target the `main` branch.
- Include two sections in the PR description:
  - **Summary** -- what the PR does and why.
  - **Test Plan** -- how the changes were verified.
- Ensure all pre-commit hooks pass before opening the PR.
