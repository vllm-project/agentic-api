# Cassette Recorder

`record_cassette.py` runs an embedded proxy between the script and an upstream API (OpenAI, vLLM, or the agentic-api gateway). Every request and response is captured into a YAML cassette for use in replay tests.

## How it works

```
[record_cassette.py] -> [proxy :7070] -> [OpenAI | vLLM | gateway]
                         (cassette written here)
```

The proxy intercepts requests and records their request bodies and responses.
Each `t<N>` in a Conversation Items API cassette is one HTTP request in execution order,
including conversation creation, Responses calls, and item operations. These cassettes
use a single flat `turns` list.

For the interactive recorder modes, the script prompts for each input message. You can type
prompts in a terminal or pipe them in with `printf` or `echo`. The `items` mode runs fixed
scenarios without prompting:

```bash
# interactive -- type each prompt when asked
python tests/cassettes/record_cassette.py --mode responses --turns 2 --no-stream --vllm http://localhost:5050 --model Qwen/Qwen3-30B-A3B-FP8 --max-output-tokens 1024 --output out.yaml

# non-interactive -- pipe prompts in (one line per turn)
printf 'First prompt\nSecond prompt\n' | python tests/cassettes/record_cassette.py --mode responses --turns 2 --no-stream --vllm http://localhost:5050 --model Qwen/Qwen3-30B-A3B-FP8 --max-output-tokens 1024 --output out.yaml

# gateway-backed cassette -- records the gateway-facing request/response
printf 'Use web search to look up potato, then summarize in one sentence.\n' | python tests/cassettes/record_cassette.py --mode responses --turns 1 --no-stream --gateway http://localhost:9000 --model openai/gpt-oss-20b --output out.yaml

# structured single-turn input -- sends the JSON string or item array from input.json
python tests/cassettes/record_cassette.py --mode responses --turns 1 --no-stream --no-store --max-output-tokens 0 --input-file input.json --model gpt-4o --output out.yaml

# structured opening turn, then a typed follow-up chained by previous_response_id
printf 'What did I just show you?\n' | python tests/cassettes/record_cassette.py --mode responses --turns 2 --no-stream --input-file input.json --model gpt-4o --output out.yaml
```

The recorder scripts (`record_reasoning_cassettes.sh`, `record_tool_call_cassettes.sh`, etc.) use `printf` to feed fixed prompts per test so no manual input is needed.

## Coding-harness CLI acceptance tests

The literal fixture at `../fixtures/claude-code-cache-control-request.json` mirrors the cache-bearing parts of a
Claude Code Messages request: multi-block `system`, a structured user message, and both `WebSearch` and client-owned
tool declarations. It includes explicit `5m` and `1h` TTLs. The Messages HTTP and loop integration tests assert that
the fixture remains unchanged through transparent proxying and every streaming and non-streaming gateway-tool round.

The fixture is hand-checked test data, not a captured cassette. A dedicated GitHub Actions matrix runs the real,
pinned Claude Code and Codex CLIs through the `agentic harness` attach commands. Claude Code replays the streaming
vLLM Messages web-search cassette through a deterministic local search backend. Codex replays the streaming vLLM
Responses reasoning cassette. Neither job contacts Anthropic, OpenAI, or a live model.

To run the same end-to-end check locally, install the pinned dependencies, build the server, and run the harness:

```bash
npm install --global '@anthropic-ai/claude-code@2.1.245' '@openai/codex@0.149.1'
python -m pip install 'PyYAML==6.0.3'
cargo build -p agentic-server --bins
bash scripts/claude-code-smoke.sh
bash scripts/codex-smoke.sh
```

Each smoke script starts a replay server and Agentic API, then invokes the installed CLI through the corresponding
`agentic harness` command. The Claude job opts `WebSearch` into gateway execution with
`MESSAGES_GATEWAY_TOOL_ALIASES=WebSearch=web_search`; it asserts the recorded answer, two Messages rounds, one search
request, a hidden `tool_result`, cache-bearing system and user blocks, and the exact Qwen model requested by Claude
Code 2.1.245. The Codex job asserts the recorded `HELLO` answer, one streaming Responses request, and the exact Qwen
model requested by Codex 0.149.1. It then runs `scripts/codex_image_smoke.py`, which attaches the committed
`images/inputs/red-blue-64.png` through both launcher modes and compares its bytes with the upstream capture. A third
run explicitly advertises text-only and requires the image to be absent. These cases replay the existing Qwen2.5-VL
single-image SSE recording unchanged; they validate client/catalog propagation, not fresh model inference.

## Modes

| Mode | Description |
|------|-------------|
| `responses` | Chains turns via `previous_response_id`. Supported with `--vllm`. Common mode for gateway-backed built-in tool cassettes. |
| `messages` | Anthropic Messages API (`/v1/messages`). Stateless: resends the full `messages` history each turn. With `--tool-outputs`, a turn following a `tool_use` feeds back matching `tool_result` blocks (keyed by tool name) instead of prompting. Supported with `--vllm`. |
| `conv` | Creates a conversation object, passes `conversation` id each turn. |
| `items` | Scripted Conversation Items API continuation, deletion, response-branch, and pagination scenarios. `--turns` is 5, 6 for `branch`, 10 for `pagination`, or 19 for `edge-cases`; each numbered step is one HTTP request. |
| `isolation` | Two independent conversations (A and B) recorded into one cassette. |
| `mixed` | Turn 1 uses `conversation` id, turns 2+ switch to `previous_response_id`. |
| `store_true_then_store_false` | Turn 1: `store=true` with conversation id. Remaining turns: `store=false`, still pass conversation id. |

## CLI options

```
--turns N              Number of turns
--output PATH          Output YAML path
--mode MODE            responses | messages | conv | items | isolation | mixed | store_true_then_store_false  (default: conv)
--stream / --no-stream Streaming or non-streaming (default: streaming)
--transport TRANSPORT  http | websocket  (default: http; WebSocket requires responses mode)
--model NAME           Model name sent in requests
--no-store             Set store=false
--vllm URL             vLLM upstream, e.g. http://localhost:8000 (responses mode only)
--gateway URL          agentic-api gateway, e.g. http://localhost:9000
--openai URL           OpenAI upstream (default https://api.openai.com)
--tools FILE           JSON file containing a tools array (responses mode only)
--tool-choice VALUE    "auto", "none", "required", or JSON e.g. '{"type":"function","name":"foo"}'
--tool-choice-sequence FILE
                       JSON array with one tool_choice value per linear Responses turn
--parallel-tool-calls / --no-parallel-tool-calls
                       Allow or forbid parallel tool calls
--tool-outputs FILE    JSON object mapping called tool names to output strings
--tool-search-output-tools FILE
                       JSON array returned for a client tool-search call
--tools-after-search FILE
                       Effective tools after normalized direct-vLLM search
--manual-item-replay   Replay bounded complete item history with --no-store; supports OpenAI, vLLM, gateway, and branches
--replay-output-source terminal | item-done
                       Select the streamed replay source (default terminal; item-done requires manual replay)
--reasoning JSON       JSON object containing Responses reasoning settings
--input-file FILE       JSON string or item array for Responses turn 1; WebSocket/branches require manual replay
--max-output-tokens N  max_output_tokens for Responses requests (default 1024; use 0 to omit)
--proxy-port PORT      Local proxy port (default 7070)
--branch-from TURN     Branch from this turn's response id (repeatable)
--branch-turn-number N First turn number for the corresponding branch (repeatable)
```

## Cassette YAML structure

Each cassette has a `turns` list. One entry is appended per request.

**Single turn (`--turns 1`, non-streaming):**

```yaml
turns:
- filename: t1
  request:
    method: POST
    path: /v1/responses
    body:
      model: Qwen/Qwen3-30B-A3B-FP8
      input: Reply with exactly one word: HELLO
      stream: false
      store: true
      max_output_tokens: 1024
    headers:
      content-type: application/json
    query_params: {}
  response:
    status_code: 200
    headers:
      content-type: application/json
    body:
      id: resp_abc123
      output: [...]
      usage: {...}
```

**Two turns (`--turns 2`, non-streaming) -- `t2` adds `previous_response_id`:**

```yaml
turns:
- filename: t1
  request:
    body:
      input: "Remember the word APPLE. Just say: OK"
      store: true
  response:
    body:
      id: resp_abc123

- filename: t2
  request:
    body:
      input: What word did I ask you to remember?
      previous_response_id: resp_abc123
  response:
    body:
      id: resp_def456
```

**Tool call turn -- `tool_choice` and `tools` appear in the request body:**

```yaml
turns:
- filename: t1
  request:
    body:
      input: What is the NVIDIA stock price?
      tool_choice: auto
      tools:
      - type: function
        name: get_stock_price
        description: ...
        parameters: {...}
  response:
    body:
      output:
      - type: function_call
        name: get_stock_price
        arguments: '{"ticker": "NVDA"}'
```

**Streaming turn -- `response.body` is replaced by `response.sse`, a list of raw SSE lines:**

```yaml
turns:
- filename: t1
  request:
    body:
      stream: true
  response:
    status_code: 200
    headers:
      content-type: text/event-stream; charset=utf-8
    sse:
    - "event: response.created\n"
    - "data: {...}\n"
    - "event: response.output_text.delta\n"
    - "data: {...}\n"
    - "event: response.completed\n"
    - "data: {...}\n"
```

## Recorder scripts

| Script | Cassettes | Backend |
|--------|-----------|---------|
| `record_text_only_cassettes.sh` | 10 text-only cassettes (responses + conv modes, streaming + non-streaming) | OpenAI (`OPENAI_API_KEY`) |
| `record_conversations_api_cassettes.sh` | 18 Conversation Items API cassettes: four history scenarios in both transports and one non-streaming edge-case sequence, each for both providers | OpenAI and gateway |
| `record_reasoning_cassettes.sh` | Matching explicit-reasoning cassettes (streaming + non-streaming) | gateway and OpenAI reference; optional direct vLLM |
| `record_opaque_reasoning.py` | Pinned stateless reasoning and function continuations, including branches (JSON, SSE, WebSocket) | OpenAI reference |
| `record_tool_call_cassettes.sh` | 8 tool-call cassettes (4 tool_choice modes x streaming + non-streaming) | vLLM |
| `record_codex_cli_tool_call_cassettes.sh` | Codex function/namespace/custom-tool matrix | gateway, vLLM, and OpenAI |
| `record_custom_tool_cassettes.sh` | Matching two-turn custom-tool flows (streaming + non-streaming) | gateway and OpenAI reference |
| `record_shell_cassettes.sh` | Four two-turn local-shell scenarios (streaming + non-streaming) | gateway and OpenAI reference |
| `record_mcp_cassettes.sh` | Native MCP counter tool discovery and calls (streaming + non-streaming) | gateway and OpenAI reference |
| `record_web_search_cassettes.sh` | Matching web-search calls (streaming + non-streaming) | gateway and OpenAI reference |
| `record_messages_tool_choice.py` | Forced `any` and named Messages searches followed by an automatic answer (JSON + SSE) | gateway's upstream traffic to vLLM |
| `record_image_input_cassettes.sh` | Matching two-turn image-input conversations (streaming + non-streaming) | gateway and OpenAI reference |
| `record_dynamo_cassettes.sh` | Stateful two-turn and client-executed function tool call cassettes (streaming + non-streaming) | NVIDIA Dynamo frontend |
| `record_dynamo_messages_cassettes.sh` | Two-round Messages web-search cassettes (streaming + non-streaming), validated before replacement | Existing NVIDIA Dynamo frontend |
| `record_sglang_cassettes.sh` | Same shared executor scenarios as Dynamo, staged validation and sanitized provenance | SGLang |
| `record_tool_search_cassettes.sh` | Four-turn mixed function/namespace client tool-search characterization; gateway blocking, HTTP/SSE, and WebSocket acceptance | gateway and OpenAI reference |

### Messages forced tool choice (vLLM)

`messages-any-*` and `messages-tool-*` in `messages/tool-choice/` record the gateway's two requests for each forced selector. The first
request forces `web_search`; after one successful search, the second carries the complete assistant/tool-result
history and `tool_choice.type=auto`. The `any` cases retain a `name` extension and allow parallel tool use; the named
cases remove `name` and retain `disable_parallel_tool_use=true`. Each exchange ends with the token from the search
result. Only the local search service is deterministic; the provider responses are captured from vLLM.

The local search service omits provider metadata. The gateway still emits one metadata object with the submitted
query in its model-facing tool output, so these recordings also cover the missing-metadata normalization contract.
The public `web_search_call` shape is unaffected by this internal metadata.

The gateway-search recordings were refreshed with vLLM `0.28.1rc1.dev850+g4be3dcf0f` and
`RedHatAI/Qwen3-Coder-Next-NVFP4` (cached model revision `27a8f16f463b9a13c91c332c40cf93e09717347e`).
Use the `qwen3_coder` tool-call parser and enable automatic tool choice when serving this model.
The recorder sends `temperature=0` and `chat_template_kwargs.enable_thinking=false` in each request.

From the repository root, build the gateway and run the scenario with the usual recorder dependencies:

```bash
cargo build -p agentic-server --bin agentic-server
python crates/agentic-server-core/tests/cassettes/record_messages_tool_choice.py \
    --binary target/debug/agentic-server --vllm http://127.0.0.1:8000 \
    --model RedHatAI/Qwen3-Coder-Next-NVFP4
cargo test -p agentic-server --test messages_tool_choice_cassette_test
```

The scenario uses the existing `record_cassette.py` proxy between the gateway and vLLM. It validates two provider
requests, one search and a completed public response per cassette. Replay tests compare every complete upstream
request to the capture, check the query/result pairing and public lifecycle, repeat on the same gateway, and assert
that Messages writes no conversation state.

The `messages-client-*` recordings exercise named **client-executed function tools** with an unused gateway tool
declared. vLLM returns `end_turn`; the gateway surfaces `tool_use`. The Anthropic SDK then executes `client_echo`,
submits the output using the returned call ID, and receives the token from that output. Each cassette has two public
requests and no gateway search. The replay tests assert the complete captured requests and response events,
including the sole public stop-reason correction, and repeat the conversation on the same gateway.

The client-tool recordings retain vLLM 0.29.0 and `Qwen/Qwen3-4B` revision
`1cfa9a7208912126459214e8b04321603b3df60c`. To regenerate those recordings, start their original provider:

```bash
vllm serve Qwen/Qwen3-4B --revision 1cfa9a7208912126459214e8b04321603b3df60c \
    --host 127.0.0.1 --port 8000 --tool-call-parser hermes --enable-auto-tool-choice \
    --reasoning-parser qwen3 --generation-config vllm --enforce-eager \
    --max-model-len 4096 --max-num-seqs 4 --gpu-memory-utilization 0.7
```

Then capture these scenarios with the gateway build:

```bash
python -m pip install anthropic==1.5.0
python crates/agentic-server-core/tests/cassettes/record_messages_tool_choice.py \
    --binary target/debug/agentic-server --vllm http://localhost:8000 --client-tool
```

### Text-only (OpenAI)

```bash
OPENAI_API_KEY=sk-... bash tests/cassettes/record_text_only_cassettes.sh
MODEL=gpt-4o-mini OPENAI_API_KEY=sk-... bash tests/cassettes/record_text_only_cassettes.sh
```

### Reasoning (gateway and OpenAI)

The default records the same explicit `reasoning` object against OpenAI and the
gateway for both response modes. The gateway fixture uses the same OpenAI model
as its reference so the comparison isolates gateway request and response
handling from model differences. Use `REASONING_RECORD_SET=gateway`,
`REASONING_RECORD_SET=openai`, or `REASONING_RECORD_SET=vllm` to record one
provider. The gateway recording requires a running gateway and reasoning-capable
upstream; the optional direct-vLLM set retains the legacy accumulator workflow.
Every selected recording is staged and validated before any final fixture is
replaced, so a failed provider or response cannot leave a partially refreshed
comparison set.

```bash
# Start the gateway against the same OpenAI ground-truth model in one terminal.
OPENAI_API_KEY=sk-... \
cargo run -p agentic-server -- \
  --llm-api-base https://api.openai.com \
  --skip-llm-ready-check

# Record the OpenAI-reference and gateway pairs from another terminal.
OPENAI_API_KEY=sk-... \
GATEWAY_URL=http://localhost:9000 \
MODEL=gpt-5.6 \
bash crates/agentic-server-core/tests/cassettes/record_reasoning_cassettes.sh

# To refresh only the gateway-facing pair instead:
REASONING_RECORD_SET=gateway \
GATEWAY_URL=http://localhost:9000 \
MODEL=gpt-5.6 \
bash crates/agentic-server-core/tests/cassettes/record_reasoning_cassettes.sh

vllm serve Qwen/Qwen3-30B-A3B-FP8 --reasoning-parser qwen3 --port 5050 > server.log 2>&1

REASONING_RECORD_SET=vllm \
VLLM_URL=http://0.0.0.0:5050 \
MODEL=Qwen/Qwen3-30B-A3B-FP8 \
bash crates/agentic-server-core/tests/cassettes/record_reasoning_cassettes.sh
```

### Pinned stateless reasoning qualification (#335)

`record_opaque_reasoning.py` targets only `https://api.openai.com/v1/responses` and
`gpt-5.4-2026-03-05`. It drives the existing recorder, then validates six captures:
text continuation and a reasoning-dependent function call, each over JSON, SSE and
WebSocket. Each capture has an initial request, a continuation and an independent
branch from the first response. All requests use `store: false`; no conversation or
`previous_response_id` is sent. WebSocket capture opens a new connection per request,
so it tests complete manual replay, not connection-local continuation caching.

Configure an API key locally; never paste a credential into a command
recorded in chat. From the repository root, with a private, Git-ignored `.env`:

```bash
capture_dir="$(mktemp -d /tmp/agentic-opaque-recordings.XXXXXX)"
uv run --no-project --python 3.12 --env-file .env \
  --with-requirements crates/agentic-server-core/tests/cassettes/recorder-requirements.txt \
  python crates/agentic-server-core/tests/cassettes/record_opaque_reasoning.py \
  --output-dir "$capture_dir"

AGENTIC_OPAQUE_CASSETTE_DIR="$capture_dir" \
  cargo test -p agentic-server-core --lib pinned_reference
```

Only promote the six recorder-generated YAML files to
`reasoning/opaque/gpt-5.4-2026-03-05/` after validation and Rust replay pass. Existing
captures are not overwritten by the script; failures leave staging evidence intact.
`--validate-only` makes no provider calls. `--scenario` and `--transport` select a
subset. Each full run makes 18 model requests, with at most 1024 output tokens per
request; there are no automatic retries. This is provider characterization, not
permission to enable the gateway's closed candidate profile.

The captured provider emits distinct opaque byte strings on `output_item.done` and
`response.completed`. Their equivalence is not assumed. The scenario explicitly
uses `--replay-output-source item-done` for SSE/WebSocket to exercise the exact
completed items retained by gateway ingestion. Raw terminal envelopes remain in the
cassette unchanged. The recorder does not reconstruct output from deltas; Rust replay
still performs the authoritative semantic lifecycle validation. JSON uses the output
array from its one response body.

General manual replay supports structured opening input, function outputs, linear
continuation, explicit branch turn numbers and extra branches. A tool-choice sequence
has one entry per recorded request, including extra branches. Checkpoints are immutable
and bounded to 64 turns, 4096 items / 4 MiB per history, and 32 MiB aggregate serialized
checkpoint data. Incomplete or failed responses never become replay checkpoints.
Payload printing is suppressed during manual replay; authorization is masked in captures.
Opaque state remains in the captured wire data, never in progress logs. WebSocket capture
also enforces 8 MiB per message, 64 MiB / 16384 events per turn and a 64 KiB handshake
ceiling. These are recorder limits, separate from gateway execution limits.

Run the offline recorder checks without credentials:

```bash
uv run --no-project --python 3.12 \
  --with-requirements crates/agentic-server-core/tests/cassettes/recorder-requirements.txt \
  python -m unittest discover -s crates/agentic-server-core/tests/cassettes -p 'test_record*.py'
```

### Tool calls (vLLM)

```bash
vllm serve Qwen/Qwen3-30B-A3B-FP8 --tool-call-parser hermes --enable-auto-tool-choice --port 5050 > server.log 2>&1

VLLM_URL=http://0.0.0.0:5050 MODEL=Qwen/Qwen3-30B-A3B-FP8 bash tests/cassettes/record_tool_call_cassettes.sh
```

### NVIDIA Dynamo (vLLM worker behind the Dynamo frontend)

Dynamo's `/v1/responses` rejects `previous_response_id` with `501`, so the recorder's own turn chaining cannot be
used. The script records turn 1 from a prompt, builds turn 2's input from turn 1's recorded assistant message (the
hydrated item history the gateway sends upstream), records it, and merges both into one cassette. See
[docs/guides/dynamo-upstream.md](../../../../docs/guides/dynamo-upstream.md) for the Dynamo launch commands.

```bash
DYNAMO_URL=http://127.0.0.1:8000 MODEL=openai/gpt-oss-20b bash tests/cassettes/record_dynamo_cassettes.sh
```

### SGLang

For SGLang launch, recording, sanitation, and replay instructions, see
[the SGLang upstream guide](../../../../docs/guides/sglang-upstream.md).

### Client tool search (OpenAI reference and gateway)

The recorder captures four turns: a search call, a linked search output followed by one loaded ordinary function
call, its linked function call output followed by one loaded namespace-member call, then that call's linked output and
the final message. The initial catalog contains several deferred ordinary functions and a namespace with several
deferred members; the search output loads exactly one ordinary function and exactly one member of that namespace.
OpenAI and gateway use public `tool_search_call`/`tool_search_output` and public `{ namespace, name }` calls. Gateway
blocking uses `store: false` full-item replay; gateway SSE/WebSocket profiles use stored continuation. The private
projection is not the gateway-to-upstream envelope.

Each profile also records a four-entry `tool_choice` sequence so inference cannot repeat a prior call or emit another
call on the final turn. OpenAI uses `required`, selected `get_weather`, `auto`, then `none`: its function selector
cannot identify a function nested in a namespace, and the stored continuation intentionally omits the repeated `tools`
parameter required by `required`, so the third-turn prompt identifies `travel.get_timezone` and the characterization
strictly rejects a wrong or multiple call. Gateway profiles use `required`, selected `get_weather`,
selected public `travel.get_timezone`, then `none`, so the gateway can resolve the namespace member to its flattened
upstream identity. The first public choice is `required` because the gateway's typed public `tool_choice` currently
has no `type: "tool_search"` selector; deferred declarations leave tool search as the only available choice on that
turn.

The checked-in set is exactly five public-contract flows: OpenAI blocking/SSE and gateway blocking/SSE/WebSocket.
HTTP uses the embedded proxy; WebSocket uses bounded direct capture. `TOOL_SEARCH_RECORD_SET=all` records those five
profiles. Use a fresh gateway database. After recording, the script runs `tool_search_characterization_test` as the
single semantic validator for the matrix.

Start the gateway with this SQLite path absent:

```bash
GATEWAY_PORT=3099 \
DATABASE_URL=sqlite:///tmp/agentic_api_tool_search_matrix.db \
V_API_BASE=http://127.0.0.1:8000 \
V_API_KEY="" \
V_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 \
./scripts/codex-start-gateway.sh
```

```bash
OPENAI_API_KEY=sk-... \
TOOL_SEARCH_RECORD_SET=all \
OPENAI_MODEL=gpt-5.6 \
GATEWAY_URL=http://127.0.0.1:3099 \
GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 \
bash crates/agentic-server-core/tests/cassettes/record_tool_search_cassettes.sh
```

### Web search (gateway and OpenAI)

The default records both providers. Use `WEB_SEARCH_RECORD_SET=gateway` or
`WEB_SEARCH_RECORD_SET=openai` to record only one side.

```bash
OPENAI_API_KEY=sk-... \
bash crates/agentic-server-core/tests/cassettes/record_web_search_cassettes.sh
```

### Image input (gateway → vLLM vision model, and OpenAI)

The reference path is client → OpenAI Responses API. The gateway path is client → Agentic API → vLLM hosting an
open-source vision model. Both paths receive the same image bytes, prompts, and tool definitions; only the model name
differs. `image_input_test.rs` replays every pair and compares request shape, completed-response structure, the
streaming event lifecycle, and the history the gateway forwards on continuation — never the model's wording or token
counts.

| Scenario | Turns | What it proves |
|---|---|---|
| `single-image` | 1 | `input_text` + inline PNG (`images/inputs/single-image.json`) reach the model unchanged |
| `multi-image` | 1 | text and two different PNGs interleave in order (`images/inputs/multi-image.json`) |
| `continuation` | 2 | a text follow-up by `previous_response_id` rehydrates the earlier image into context |
| `tool-image` | 2 | the model calls `view_image`, the client returns a `function_call_output` whose `output` is a content array carrying the PNG, and the model answers from it |

Each scenario is recorded streaming and non-streaming per provider (16 cassettes). Every recording is validated
(fixture bytes preserved, `previous_response_id` chained, exactly one `view_image` call answered by a structured
output) and staged before any final fixture is replaced. To change an image, replace the PNG and regenerate the JSON
turns from it; the script refuses to record when they disagree.

**Recorded configuration.** vLLM 0.29.0 serving `Qwen/Qwen2.5-VL-3B-Instruct` in bfloat16 on one 12 GB GPU
(RTX 4080 Laptop, WSL2). The stock Qwen2.5-VL chat template renders images but has no `tools` block, so with
`tool_choice: auto` the model never sees declared functions; `images/qwen2.5-vl-hermes-tools.jinja` adds the
Hermes-style tools prompt and `<tool_call>` history rendering from Qwen2.5-Instruct while keeping the multimodal
rendering, including images inside tool responses. It must be passed with `--chat-template`.

```bash
# 1. Serve the vision model. VLLM_WSL2_ENABLE_PIN_MEMORY is needed under WSL2 only;
#    VLLM_USE_FLASHINFER_SAMPLER=0 avoids a JIT build when no CUDA toolkit (nvcc) is installed.
VLLM_WSL2_ENABLE_PIN_MEMORY=1 VLLM_USE_FLASHINFER_SAMPLER=0 \
vllm serve Qwen/Qwen2.5-VL-3B-Instruct \
  --dtype bfloat16 --max-model-len 8192 --max-num-seqs 2 \
  --gpu-memory-utilization 0.82 --enforce-eager \
  --limit-mm-per-prompt '{"image": 4}' \
  --mm-processor-kwargs '{"max_pixels": 200704}' \
  --enable-auto-tool-choice --tool-call-parser hermes \
  --chat-template crates/agentic-server-core/tests/cassettes/images/qwen2.5-vl-hermes-tools.jinja \
  --port 8000

# 2. Start the gateway against it.
cargo run -p agentic-server -- --llm-api-base http://127.0.0.1:8000

# 3. Record the OpenAI reference and the gateway set.
OPENAI_API_KEY=sk-... \
GATEWAY_URL=http://localhost:9000 \
MODEL=Qwen/Qwen2.5-VL-3B-Instruct \
bash crates/agentic-server-core/tests/cassettes/record_image_input_cassettes.sh
```

Use `IMAGE_RECORD_SET=gateway` or `IMAGE_RECORD_SET=openai` to record one provider, `IMAGE_SCENARIOS="tool-image"`
(space-separated) to record a subset, and `OPENAI_MODEL` to change the reference model. A different gateway model
changes the cassette file names; update `GATEWAY_MODEL` and `GATEWAY_MODEL_SLUG` in `image_input_test.rs` to match.
The `tool-image` scenario uses `tool_choice: auto` so the recording proves the model chose to call the tool; if a
small model answers without calling it, validation fails and the scenario can simply be re-run.

### Custom tool (gateway and OpenAI)

This records an unformatted freeform custom tool, including the
`custom_tool_call_output` continuation. Grammar-constrained custom tools are
covered separately by unit tests because the gateway intentionally rejects
formats that normalization cannot preserve.

```bash
OPENAI_API_KEY=sk-... \
bash crates/agentic-server-core/tests/cassettes/record_custom_tool_cassettes.sh
```

Use `CUSTOM_TOOL_RECORD_SET=gateway` or `CUSTOM_TOOL_RECORD_SET=openai` to
record only one provider.

### Shell (gateway and OpenAI)

See [the shell recording guide](shell/README.md) for branch build, gateway startup,
and recording commands. Each scenario records two requests: a shell call, then
matching structured `shell_call_output` and a follow-up user message chained with
`previous_response_id`. The cases cover successful output, stderr with a nonzero
exit code, timeout, and multiple commands with ordered outputs. Both streaming
and non-streaming modes are recorded for each provider.

The client command outputs are simulated by `shell/scenarios.py`; no commands are
executed. The requests and model responses are captured live by the standard
recorder. Each recording is written directly to its output YAML, which is retained
if recording or validation fails. Streaming validation requires command added/delta/done
events. Use `SHELL_RECORD_SET=gateway`, `openai`, or `all` (default).

The shared recorder's `--tool-outputs` option accepts a Python `shell(action=...)`
callback or a JSON `shell` key containing an object with `output` and optional
`max_output_length`. It builds `shell_call_output` using the actual `call_id` and
preserves the structured stdout/stderr/outcome entries.

### Codex custom tools (gateway, vLLM, and OpenAI)

The custom fixture uses a Lark grammar and records two turns: the model returns raw `custom_tool_call.input`, then the
recorder submits the matching `custom_tool_call_output` before the follow-up user message.

```bash
GATEWAY_URL=http://127.0.0.1:3018 \
V_MODEL=Qwen/Qwen3.6-35B-A3B \
bash tests/cassettes/record_codex_cli_tool_call_cassettes.sh gateway-custom

VLLM_URL=http://127.0.0.1:8000 \
V_MODEL=Qwen/Qwen3.6-35B-A3B \
bash tests/cassettes/record_codex_cli_tool_call_cassettes.sh direct-vllm-custom

OPENAI_API_KEY=sk-... \
OPENAI_CUSTOM_MODEL=gpt-5.6 \
bash tests/cassettes/record_codex_cli_tool_call_cassettes.sh openai-custom
```

### Conversation Items API (OpenAI and gateway)

`record_conversations_api_cassettes.sh` uses the shared `record_cassette.py` proxy to record
four history scenarios against each provider, with streaming and non-streaming Responses calls,
plus one non-streaming item API edge-case sequence. Each
`filename: tN` is one HTTP request in a flat `turns` list; there is no `setup` or
`after_turn` section. The recorder writes the captured YAML directly.

The four history scenarios start with t1 `POST /v1/conversations`, t2 `POST /v1/responses`
using the conversation ID to remember SAPPHIRE, and t3
`POST /v1/conversations/{conversation_id}/items` to add ORCHID.

| Scenario | Remaining requests and expected history |
|----------|-----------------------------------------|
| `continuation` | t4 continues through the conversation ID and answers ORCHID; t5 lists the conversation items, including that response. |
| `deletion` | t4 deletes the ORCHID item; t5 lists the conversation items without it. |
| `branch` | t4 branches through `previous_response_id` alone and answers SAPPHIRE; t5 adds VIOLET through the conversation items path; t6 lists the conversation items. The branch response does not appear in that list. The Responses call never combines `conversation` with `previous_response_id`. |
| `pagination` | t4 lists all items ascending; t5–t6 list ascending pages of size two, with t6 using t5's `last_id` as `after`. t7 lists all items descending; t8–t9 repeat the cursor check in descending order. t10 lists ascending with `include[]=message.output_text.logprobs`. |
| `edge-cases` | One YAML: t1–t3 try a message with client-supplied `item_wrongprefix_…` and list; t4–t6 try a function call with `msg_wrongprefix_…` and valid `call_id`, then list; t7–t11 add three valid messages, list two, delete the page cursor, and list with the deleted ID as `after`. t12–t14 copy one surviving generated ID into a fresh conversation as a control; t15–t17 submit that ID twice with different content into another fresh conversation; t18–t19 reuse it in its original conversation. Each insertion probe is followed by a list to capture actual state. Probe responses are recorded without assuming a status. |

Run from the repository root with `OPENAI_API_KEY` set and the gateway, its database,
and vLLM running. Set `GATEWAY_MODEL` if the gateway model differs from
`OPENAI_MODEL` (default `gpt-4.1`); `PYTHON_BIN` selects the Python environment
with the packages in `recorder-requirements.txt`.

```bash
bash crates/agentic-server-core/tests/cassettes/record_conversations_api_cassettes.sh

# Re-record only the pagination pair, for both providers and both transports.
CONVERSATIONS_SCENARIO=pagination bash crates/agentic-server-core/tests/cassettes/record_conversations_api_cassettes.sh
CONVERSATIONS_SCENARIO=pagination-stream bash crates/agentic-server-core/tests/cassettes/record_conversations_api_cassettes.sh

# Record all gateway scenarios, including the combined edge-case YAML.
CONVERSATIONS_RECORD_SET=gateway bash crates/agentic-server-core/tests/cassettes/record_conversations_api_cassettes.sh
```

`CONVERSATIONS_RECORD_SET=openai` or `gateway` selects one provider;
`CONVERSATIONS_SCENARIO=edge-cases` records the complete nineteen-request edge-case sequence into one YAML per provider.
Only the four history scenarios
have `-stream` variants. The prefix probes send explicit wrong-prefix IDs, whereas
normal item creation lets the provider assign IDs. The edge-case status codes
are learned from the OpenAI recording, then compared with the gateway recording.
The existing Rust history comparison requires both recordings for its four history scenarios.
The edge-case assertions cover the nineteen-step recordings from both providers. The new OpenAI recording establishes that:

- Reusing a generated item ID in another conversation returns 200 and retains the original item's content, ignoring the submitted replacement content.
- Supplying that ID twice in one request to a fresh conversation returns 200 and lists two occurrences with the same public ID and original content.
- Reusing it in its original conversation returns 400 with `type: invalid_request_error`, `code: item_already_in_conversation`, `param: items`, and message `Item already in conversation`; the original history stays unchanged.

The latest gateway recording matches all nineteen OpenAI steps: status codes, full response bodies,
item ordering, and ID relationships, with dynamic IDs and timestamps normalized. Deleted-cursor
pagination returns 404 with `type: invalid_request_error`, `param: after`, null `code`, and
`No item found with id '<deleted_id>'`. The message and function-call prefix probes return 400
with `code: invalid_value`, `param: items[0].id`, and the recorded expected-prefix message.

The tests compare every recorded edge-case response without skipping the previously failing
steps. They also replay all nineteen OpenAI requests against gateway handlers backed by a fresh
SQLite database. Storage regressions separately exercise repeated references across SQL batches,
rejection in later requests, rollback, tenant isolation, and preserved response snapshots.
The storage model keeps each history occurrence's internal primary key separate from the reused
public item ID; uniqueness of `(conversation_id, public_item_id)` would reject the accepted t16 case.

These probes concern existing generated IDs. They do not establish OpenAI behavior for duplicate
invented IDs, pagination/deletion among repeated occurrences, or every item type's prefix.
Those cases require additional recordings before claiming parity.

Run the recording assertions with:

```bash
REQUIRE_CONVERSATIONS_CASSETTES=1 cargo test -p agentic-server-core --test conversations_api_cassette_test
```

The assertions check successful statuses, request routes and ID relationships,
secret-word answers, conversation history visibility, and the SSE event lifecycle.
They compare each provider's ascending and descending pages with its own full
ordered item list, including cursor, `first_id`, `last_id`, and `has_more`.
Gateway reasoning items can add raw items and shift page boundaries, so the
test does not require OpenAI and gateway pages to contain the same raw items.
The `include` request is checked for success and unchanged item order and
message shape; this text-only scenario does not exercise other `include`
expansions or every Conversation Items API format.

### Compaction replay (OpenAI)

These recordings capture the non-streaming `/v1/responses` inference calls replayed by the compaction integration
tests. The JSON inputs contain the exact model-facing item arrays, including the context-checkpoint prompt. Use the
existing recorder directly from `crates/agentic-server-core`:

```bash
record_compaction() {
  input_name="$1"
  output_name="$2"
  uv run \
    --with click \
    --with fastapi \
    --with httpx \
    --with uvicorn \
    --with pyyaml \
    python tests/cassettes/record_cassette.py \
    --mode responses \
    --turns 1 \
    --no-stream \
    --no-store \
    --max-output-tokens 0 \
    --openai https://api.openai.com \
    --model gpt-4o \
    --input-file "tests/cassettes/compaction/inputs/${input_name}.json" \
    --output "tests/cassettes/compaction/compact-${output_name}-gpt-4o-nonstreaming.yaml"
}

export OPENAI_API_KEY=sk-...
record_compaction basic basic
record_compaction tool-prior-compaction tool-prior
record_compaction followup followup
```
