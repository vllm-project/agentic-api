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

## Reporting Issues

Use the issue templates provided on the
[GitHub Issues](https://github.com/vllm-project/agentic-api/issues) page. Choose the
template that best matches your report (bug report, feature request, etc.).

## Code of Conduct

This project follows a Code of Conduct. Please review
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for details on expected behavior.
