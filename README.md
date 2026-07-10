# agentic-api
Stateful API logic for agentic applications using vLLM

A Rust-first project that is migrating agentic gateway functionality from Python into
native Rust components. The previous Python gateway implementation has been removed.
Design and migration decisions are tracked in the ADRs under `docs/adr/`.

## Repository layout

- Rust source: `src/`
- Rust package manifest: `Cargo.toml`
- Documentation: `docs/`

## Build

```bash
cargo build
```

## Test

```bash
cargo test
```

## Web search

The stateful `/v1/responses` executor supports OpenAI-compatible `web_search`
tool declarations by normalizing them into a `web_search` function call for
vLLM. Set `YOU_API_KEY` and `YOU_API_BASE_URL` to enable execution through
You.com's Search API.

## Lint and format

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

## Documentation

```bash
uv venv
uv pip install -r docs/requirements.txt
uv run mkdocs serve
```
