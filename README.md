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

## Using agentic-api with Codex

1. Start a vLLM server using any recipe from [recipes.vllm.ai](https://recipes.vllm.ai), for example:

   ```bash
   vllm serve Qwen/Qwen3-30B-A3B-FP8 --tool-call-parser qwen3_coder --enable-auto-tool-choice --reasoning-parser qwen3 --port 5050
   ```

2. Run `agentic-api`, pointing it at the vLLM server. Set `YOU_API_KEY` and `YOU_API_BASE_URL` to also enable web search:

   ```bash
   YOU_API_KEY=<your-you.com-api-key> YOU_API_BASE_URL=<you.com-api-base-url> \
     cargo run -p agentic-server -- --llm-api-base http://0.0.0.0:5050
   ```

3. Configure Codex to use `agentic-api` as a model provider. Create a `config.toml` in `<codex-home-path>/.codex`:

   ```toml
   [model_providers.agentic-api]
   name = "agentic-api"
   base_url = "http://localhost:9000/v1" # point to the agentic-api gateway url
   wire_api = "responses"
   requires_openai_auth = false
   supports_websockets = true
   ```

4. Run Codex against `agentic-api`:

   ```bash
   CODEX_HOME=/codex-home-path/.codex codex --disable image_generation -c model_provider=agentic-api -m Qwen/Qwen3-30B-A3B-FP8
   ```

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
