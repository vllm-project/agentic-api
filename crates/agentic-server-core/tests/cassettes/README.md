# Cassette Recorder

`record_cassette.py` runs an embedded proxy between the script and an upstream API (OpenAI, vLLM, or the agentic-api gateway). Every request and response is captured into a YAML cassette for use in replay tests.

## How it works

```
[record_cassette.py] -> [proxy :7070] -> [OpenAI | vLLM | gateway]
                         (cassette written here)
```

The proxy intercepts each turn, records the request body and response, then appends a `t<N>` entry to the output YAML.

The recorder is interactive. For each turn it prompts you to type the input message and waits for you to press Enter before sending the request. You can run it directly in your terminal and type the prompts by hand, or pipe them in from a script using `printf` or `echo` to feed all turns non-interactively:

```bash
# interactive -- type each prompt when asked
python tests/cassettes/record_cassette.py --mode responses --turns 2 --no-stream --vllm http://localhost:5050 --model Qwen/Qwen3-30B-A3B-FP8 --max-output-tokens 1024 --output out.yaml

# non-interactive -- pipe prompts in (one line per turn)
printf 'First prompt\nSecond prompt\n' | python tests/cassettes/record_cassette.py --mode responses --turns 2 --no-stream --vllm http://localhost:5050 --model Qwen/Qwen3-30B-A3B-FP8 --max-output-tokens 1024 --output out.yaml

# gateway-backed cassette -- records the gateway-facing request/response
printf 'Use web search to look up potato, then summarize in one sentence.\n' | python tests/cassettes/record_cassette.py --mode responses --turns 1 --no-stream --gateway http://localhost:9000 --model openai/gpt-oss-20b --output out.yaml
```

The recorder scripts (`record_reasoning_cassettes.sh`, `record_tool_call_cassettes.sh`, etc.) use `printf` to feed fixed prompts per test so no manual input is needed.

## Modes

| Mode | Description |
|------|-------------|
| `responses` | Chains turns via `previous_response_id`. Supported with `--vllm`. Common mode for gateway-backed built-in tool cassettes. |
| `messages` | Anthropic Messages API (`/v1/messages`). Stateless: resends the full `messages` history each turn. With `--tool-outputs`, a turn following a `tool_use` feeds back matching `tool_result` blocks (keyed by tool name) instead of prompting. Supported with `--vllm`. |
| `conv` | Creates a conversation object, passes `conversation` id each turn. |
| `isolation` | Two independent conversations (A and B) recorded into one cassette. |
| `mixed` | Turn 1 uses `conversation` id, turns 2+ switch to `previous_response_id`. |
| `store_true_then_store_false` | Turn 1: `store=true` with conversation id. Remaining turns: `store=false`, still pass conversation id. |

## CLI options

```
--turns N              Number of turns
--output PATH          Output YAML path
--mode MODE            responses | conv | isolation | mixed | store_true_then_store_false  (default: conv)
--stream / --no-stream Streaming or non-streaming (default: streaming)
--model NAME           Model name sent in requests
--no-store             Set store=false
--vllm URL             vLLM upstream, e.g. http://localhost:8000 (responses mode only)
--gateway URL          agentic-api gateway, e.g. http://localhost:9000
--openai URL           OpenAI upstream (default https://api.openai.com)
--tools FILE           JSON file containing a tools array (responses mode only)
--tool-choice VALUE    "auto", "none", "required", or JSON e.g. '{"type":"function","name":"foo"}'
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
| `record_reasoning_cassettes.sh` | 2 reasoning cassettes (single turn, streaming + non-streaming) | vLLM |
| `record_tool_call_cassettes.sh` | 8 tool-call cassettes (4 tool_choice modes x streaming + non-streaming) | vLLM |

### Text-only (OpenAI)

```bash
OPENAI_API_KEY=sk-... bash tests/cassettes/record_text_only_cassettes.sh
MODEL=gpt-4o-mini OPENAI_API_KEY=sk-... bash tests/cassettes/record_text_only_cassettes.sh
```

### Reasoning (vLLM)

```bash
vllm serve Qwen/Qwen3-30B-A3B-FP8 --reasoning-parser deepseek_r1 --port 5050 > server.log 2>&1

VLLM_URL=http://0.0.0.0:5050 MODEL=Qwen/Qwen3-30B-A3B-FP8 bash tests/cassettes/record_reasoning_cassettes.sh
```

### Tool calls (vLLM)

```bash
vllm serve Qwen/Qwen3-30B-A3B-FP8 --tool-call-parser hermes --enable-auto-tool-choice --port 5050 > server.log 2>&1

VLLM_URL=http://0.0.0.0:5050 MODEL=Qwen/Qwen3-30B-A3B-FP8 bash tests/cassettes/record_tool_call_cassettes.sh
```
