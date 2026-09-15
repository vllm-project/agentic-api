# Running Agentic API in front of NVIDIA Dynamo

[NVIDIA Dynamo](https://github.com/ai-dynamo/dynamo) is a distributed inference serving framework. Its frontend
exposes an OpenAI-compatible HTTP API, including `/v1/responses`, and routes requests to backend workers. Dynamo ships
its own workers (vLLM, SGLang, TensorRT-LLM); the vLLM worker (`python -m dynamo.vllm`) embeds the vLLM engine, so a
Dynamo deployment is a complete inference stack on its own. You do not run `vllm serve` alongside it.

This guide records how to put Agentic API in front of a Dynamo deployment and what differs from pointing the gateway
at a standalone `vllm serve`.

The short version: nothing in Agentic API needs to change. Start the gateway with `--llm-api-base` pointing at the
Dynamo frontend and it works, including stateful `previous_response_id` chaining and client-executed function tools.

## What Dynamo does and does not provide

| Capability | Dynamo frontend | Agentic API adds |
|---|---|---|
| `POST /v1/responses`, `/v1/chat/completions`, `/v1/models`, `/health` | ✅ | — |
| Reasoning / tool-call parsing | ✅ via `--dyn-reasoning-parser` / `--dyn-tool-call-parser` on the worker | — |
| `previous_response_id` | ❌ returns `501 Not Implemented` (`Validation: previous_response_id is not supported.`) | ✅ Stores every response and rehydrates the full item history on each turn, so the upstream call is stateless |
| Gateway-executed built-in tools (web search, MCP), background execution, WebSocket transport | ❌ | ✅ |

Because Dynamo rejects `previous_response_id`, the gateway never forwards it. The second turn of a conversation reaches
Dynamo as one `input` array containing the earlier user message, the stored assistant message, and the new user
message. The replay tests in `crates/agentic-server-core/tests/dynamo_cassette_test.rs` assert exactly that shape.

## 1. Install Dynamo

Dynamo publishes wheels on PyPI. The `[vllm]` extra pulls in the vLLM version Dynamo's worker is built against, so use
a dedicated virtual environment, and pin the Dynamo release:

```bash
mkdir -p ~/dev/dynamo && cd ~/dev/dynamo
uv venv --python 3.12 .venv
VIRTUAL_ENV=$PWD/.venv uv pip install "ai-dynamo[vllm]==1.4.1"
```

Pin the version. Dynamo's README suggests `uv pip install --prerelease=allow "ai-dynamo[vllm]"`, but that flag lets uv
resolve *any* dependency to a pre-release and, with an unpinned `ai-dynamo`, installs the latest `1.5.0.devYYYYMMDD`
build rather than a release. Check what you got with `python -c 'import importlib.metadata as m; print(m.version("ai-dynamo"))'`.

This guide was verified with `ai-dynamo==1.4.1` (which installs `vllm==0.26.0` and `torch` cu130) on an aarch64 host
with a single GB10 GPU. See Dynamo's [release artifacts](https://docs.nvidia.com/dynamo/resources/release-artifacts)
and [support matrix](https://docs.nvidia.com/dynamo/resources/support-matrix) for the wheel/CUDA combinations of other
releases. File discovery can be used for a manually managed single-host setup. The Messages recordings used
etcd discovery because the file watcher did not deliver model registrations on the tested container host.

## 2. Start the Dynamo frontend and a worker

Run each in its own terminal (or tmux window). The frontend is the HTTP entry point (default port 8000, round-robin
routing across whatever workers register); the worker loads the model. Models are resolved from the Hugging Face
cache, so anything already downloaded for vLLM is reused.

```bash
# Frontend: OpenAI-compatible HTTP on :8000
.venv/bin/python -m dynamo.frontend --http-port 8000 --discovery-backend file

# Worker: vLLM engine managed by Dynamo
.venv/bin/python -m dynamo.vllm \
  --model openai/gpt-oss-20b \
  --discovery-backend file \
  --kv-events-config '{"enable_kv_cache_events": false}' \
  --dyn-reasoning-parser gpt_oss \
  --dyn-tool-call-parser harmony \
  --max-model-len 32768
```

`openai/gpt-oss-20b` needs roughly 16 GB of GPU memory for weights plus KV cache, so it fits a single 24 GB GPU with
vLLM's default `--gpu-memory-utilization 0.9`. Lower that only when the GPU is shared (see below).

Flags worth knowing:

| Flag | Why |
|---|---|
| `--discovery-backend file` | Lets the frontend and worker find each other via `/tmp/dynamo_store_kv` instead of etcd. Pass it to both. |
| `--kv-events-config '{"enable_kv_cache_events": false}'` | Required for the vLLM worker without NATS. |
| `--dyn-reasoning-parser` / `--dyn-tool-call-parser` | The Dynamo *frontend* parses model output, not vLLM. vLLM's `--reasoning-parser` is ignored and `--tool-call-parser` / `--enable-auto-tool-choice` are rejected as unknown arguments. Without the `--dyn-*` flags, gpt-oss "analysis" text leaks into `content` and tool calls come back as plain text. `gpt_oss` + `harmony` is the verified pair for gpt-oss models; `python -m dynamo.vllm --help` lists the parsers for other model families. |
| `--gpu-memory-utilization` | Standard vLLM engine flag (the worker accepts vLLM engine arguments). It is a fraction of *total* device memory and must fit in the memory currently free, or the engine fails at startup. On a dedicated GPU keep the default; on a shared GPU size it to what is actually free (the recordings for this guide used `0.15` on a 121 GB unified-memory host that was also running another model). |

Confirm the worker registered and parsing works:

```bash
curl -s localhost:8000/v1/models | jq '.data[].id'
curl -s localhost:8000/v1/chat/completions -H 'Content-Type: application/json' -d '{
  "model": "openai/gpt-oss-20b",
  "messages": [{"role": "user", "content": "Say hello in five words."}],
  "max_tokens": 500
}' | jq '.choices[0].message | {content, reasoning_content}'
```

`content` should hold the answer and `reasoning_content` the chain of thought. If the answer starts with `analysis`, the
worker is missing `--dyn-reasoning-parser`.

## 3. Start Agentic API against the Dynamo frontend

```bash
cargo build -p agentic-server --bins
./target/debug/agentic-server --llm-api-base http://127.0.0.1:8000
```

Agentic API's startup probe uses Dynamo's `/health`, so no `--skip-llm-ready-check` is needed. Be aware of what that
probe means: Dynamo returns `200 {"status":"healthy", ...}` as soon as the frontend's HTTP service is up, even with no
worker registered (the `instances` list is simply empty). It does **not** mean a model is loaded. Wait for the
per-model readiness endpoint before sending traffic:

```bash
curl -s localhost:8000/v1/models/openai%2Fgpt-oss-20b   # 404 until the worker registers, then model metadata
```

The harness CLI works the same way: `./target/debug/agentic run codex --upstream http://127.0.0.1:8000`.

## 4. Verify a stateful conversation and a tool call

```bash
R1=$(curl -s localhost:9000/v1/responses -H 'Content-Type: application/json' -d '{
  "model": "openai/gpt-oss-20b",
  "input": "Remember the word APPLE. Just say: OK",
  "max_output_tokens": 2048
}')
echo "$R1" | jq -r '.output[] | select(.type=="message") | .content[0].text'   # OK

curl -s localhost:9000/v1/responses -H 'Content-Type: application/json' -d "{
  \"model\": \"openai/gpt-oss-20b\",
  \"input\": \"What word did I ask you to remember? Reply with just the word.\",
  \"previous_response_id\": \"$(echo "$R1" | jq -r .id)\",
  \"max_output_tokens\": 2048
}" | jq -r '.output[] | select(.type=="message") | .content[0].text'           # APPLE

curl -s localhost:9000/v1/responses -H 'Content-Type: application/json' -d '{
  "model": "openai/gpt-oss-20b",
  "input": "What is the current NVIDIA stock price? Use the tool.",
  "max_output_tokens": 2048,
  "tools": [{"type": "function", "name": "get_stock_price",
             "description": "Get the latest stock price for a ticker symbol",
             "parameters": {"type": "object", "properties": {"ticker": {"type": "string"}}, "required": ["ticker"]}}]
}' | jq '.output[] | select(.type=="function_call") | {name, arguments}'
```

Expected: `OK`, then `APPLE`, then a `get_stock_price` call with `{"ticker":"NVDA", ...}`.

Sending the second request straight to Dynamo (port 8000) instead of the gateway fails with `501`; that difference is
the value the gateway adds. The third request exercises a client-executed function tool: Dynamo returns the function
call and the application runs it.

## Recorded cassettes and CI

The interactions above are recorded in `crates/agentic-server-core/tests/cassettes/dynamo/` and replayed by
`tests/dynamo_cassette_test.rs` on every `cargo test`, so CI covers the Dynamo upstream without a GPU. The
`dynamo-upstream` CI job also runs `scripts/validate-cassettes.py`, a structural check over every recorded cassette. To re-record
against a live Dynamo (for example after a Dynamo release changes the response shape):

```bash
cd crates/agentic-server-core
DYNAMO_URL=http://127.0.0.1:8000 MODEL=openai/gpt-oss-20b \
  bash tests/cassettes/record_dynamo_cassettes.sh
```

The script records the second stateful turn from the hydrated item history the gateway would send (built from turn
1's recorded assistant message), because the recorder's own `previous_response_id` chaining cannot be used against a
stateless upstream.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `unrecognized arguments: --tool-call-parser` | Use `--dyn-tool-call-parser` on the worker. |
| Answer text begins with `analysis…assistantfinal…` | Add `--dyn-reasoning-parser gpt_oss` (or the parser for your model). |
| `Free memory on device … is less than desired GPU memory utilization` | Lower `--gpu-memory-utilization`; it is a fraction of total memory. |
| `CUDA error: out of memory` right after restarting a worker | A previous `dynamo.vllm` process is still alive and holding memory; `pkill -f "python -m dynamo.vllm"` before relaunching. Closing its terminal or tmux window does not kill it. |
| `501 Validation: previous_response_id is not supported.` | You are calling Dynamo directly. Send the request to the gateway. |
| Gateway logs `LLM ready` but requests fail with no model / `model not found` | `/health` is green before the worker registers. Check `/v1/models` or `/v1/models/{model}` and the worker log. |
| Installed version is `1.5.0.dev…` | `--prerelease=allow` with an unpinned `ai-dynamo` picked a dev build. Reinstall with `"ai-dynamo[vllm]==1.4.1"`. |

## Messages recordings (#213)

The streaming and non-streaming Messages recordings were captured from Dynamo 1.4.1 with vLLM 0.26.0 on Linux
x86_64, an NVIDIA L40S 46,068 MiB GPU, and NVIDIA driver 580.159.04. The model snapshot was
`6cee5e81ee83917806bbde320786a8fb61efebee`. The checked-in acceptance test replays both real captures on every run.

The frontend must enable the experimental Anthropic endpoint with `--enable-anthropic-api`. The gateway sends
`/v1/messages` to that endpoint; it does not convert Messages requests into Chat Completions itself. Check this flag
with the pinned frontend's `--help` before recording. Keep the worker's `--dyn-reasoning-parser gpt_oss` and
`--dyn-tool-call-parser harmony` settings.

### Offline replay

The regular Rust CI job runs this integration test through `cargo test`. It replays the checked-in Dynamo responses
through the production Messages tool loop and compares the next upstream request with the recorded history.
No GPU, running Dynamo service, Python environment, or external search service is required:

```bash
cargo test --locked -p agentic-server-core --test dynamo_messages_test
```

The fixed tool output comes from `messages/tool_outputs.json`; it is not a claim about today's Rust release.
The legacy vLLM preparation fixture omitted a thinking signature in its next request. Its test recovers that
expectation from the recorded event; the Dynamo acceptance test compares the recorded history directly.

### Refresh the recordings

Provision and start Dynamo separately, with the Messages endpoint and parsers described above. From the repository
root, install the recorder dependencies in a separate environment:

```bash
uv venv --python 3.12 .venv-dynamo-recorder
uv pip install --python .venv-dynamo-recorder/bin/python \
  -r crates/agentic-server-core/tests/cassettes/recorder-requirements.txt

PYTHON="$PWD/.venv-dynamo-recorder/bin/python" \
DYNAMO_URL=http://127.0.0.1:8000 MODEL=openai/gpt-oss-20b \
bash crates/agentic-server-core/tests/cassettes/record_dynamo_messages_cassettes.sh

cargo test --locked -p agentic-server-core --test dynamo_messages_test
```

The script records against an existing endpoint; it does not install or manage Dynamo. Both modes must pass
scenario and structural validation before replacing the destination files. Failed runs retain their staging
directory for diagnosis. Do not hand-edit captured YAML. The recorder preserves `signature_delta` content and
fails on malformed tool argument JSON instead of substituting an empty object.

The recording utilities have separate, GPU-independent tests for signature preservation, malformed arguments,
two-round history, and preserving installed fixtures when recording fails. Run these when changing the recorder:

```bash
.venv-dynamo-recorder/bin/python -m unittest discover \
  -s crates/agentic-server-core/tests/cassettes -p 'test_messages.py' -v
```
