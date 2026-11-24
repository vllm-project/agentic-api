# Responses transport and agentic-workflow benchmark

The benchmark target at `crates/agentic-server/benches/client/response_provider/` runs native Responses API clients against:

- Agentic API over one persistent WebSocket per session.
- Agentic API over HTTP with server-sent events (SSE).
- Agentic API over non-streaming HTTP/JSON.
- vLLM directly over HTTP/SSE.
- vLLM directly over non-streaming HTTP/JSON.

It does not launch Codex CLI and does not use MCP. Function definitions from BFCL are sent directly in each Responses
request's `tools` array. Transport benchmark functions are executed locally in-process, so MCP discovery, process
startup, and stdio IPC are not included in latency measurements.

By default, the runner starts 5 sessions per selected provider at one synchronization barrier. Requests within one
session remain sequential, while all sessions run concurrently.

## Workloads

| Workload | What it measures | Default turns/session |
| --- | --- | ---: |
| `transport` | Repeated model/function/model rounds over a reused connection. | 1 |
| `tool-call` | BFCL function selection and argument accuracy. | 1 |
| `history-rehydration` | Accuracy when each turn depends on the preceding stored response. | 10 |

For Agentic API sessions, continuation requests contain `previous_response_id` and only the new input items. The
gateway rehydrates stored item history. Direct vLLM sessions have no gateway state, so the benchmark replays the full
accumulated Responses item history in each request. This makes wire-size and history-management differences part of
the deployment comparison.

### Common execution model

`SESSIONS` controls concurrency and `REQUESTS_PER_SESSION` controls the number of sequential turns in each session.
Every provider/session task connects first and waits at the same barrier, so the configured sessions begin under
approximately the same load. A session then completes turn N before starting turn N+1. For example, 5 sessions with
50 requests per session produces 250 planned turns per provider, with at most 5 turns in flight at once.

For `history-rehydration`, `DEPTHS` (a comma-separated list, e.g. `5,10,25`) overrides `REQUESTS_PER_SESSION`: each
depth runs as its own independent batch of `SESSIONS` sessions instead of one long run sliced into buckets
afterward, so sample counts stay even across depths. It also produces `depth_summary.md`/`.json` with request bytes,
response bytes, latency, and accuracy grouped by depth.

`SESSIONS` may also be a comma-separated list, such as `1,5,10,25,50`. The runner script then runs one independent
batch per level, in order, each under `sessions-NNNNN/` with its own `summary.md`, and lists the levels in
`sessions_summary.md`. With `GATEWAY_PID` set to the process ID of a local Agentic API server (Linux), the script
reads the server's CPU time and resident memory from `/proc` once a second during each batch (`gateway-usage.csv`),
and `sessions_summary.md` gives each level's gateway CPU seconds, mean cores in use and peak RSS: the gateway's own
cost as load grows, which client-side latency does not show.

The WebSocket provider keeps one connection open for the complete session. HTTP providers reuse an HTTP client, but
each model response remains a separate Responses request. The benchmark generates prompts once and assigns the same
session and turn indices to every selected provider, allowing successful gateway and direct-vLLM turns to be paired.
Use the same `SEED`, BFCL files, and `DATASET_OFFSET` when the two provider arms are run separately.

There are two kinds of session state:

- `transport` and `tool-call` reset conversation state after every logical turn. Repeating these workloads measures
  connection reuse and sustained concurrent traffic without allowing one case to affect another.
- `history-rehydration` deliberately retains conversation state between turns. A broken response,
  timeout, or provider error stops that session because the next turn would no longer have a trustworthy continuation.

Rates use all planned turns as their denominator. Therefore, turns skipped after a session-ending failure reduce the
success and correctness rates; `attempted_turns` in `run.json` shows how many requests were actually started. Latency
distributions include only successful turns. Always read latency together with success and correctness: a provider
must not look faster merely because its slow or difficult turns failed.

### Choosing a comparison

| Question | Workload and providers | Most useful observations |
| --- | --- | --- |
| What does persistent WebSocket mode change? | Any workload; `agentic-api` versus `agentic-api-http`. | First-output latency, end-to-end latency, continuation-round latency, throughput. |
| What is the gateway cost with the same streaming transport? | Any workload; `agentic-api-http` versus `vllm`. | Paired latency deltas, success, input tokens, throughput. |
| What is the streaming benefit over complete JSON responses? | Same endpoint; SSE/WebSocket versus its JSON provider. | First-output latency and TTFT for streaming, end-to-end latency for both. |
| Does the model emit correct function calls? | `tool-call`; the same BFCL category on both providers. | Tool-call accuracy, success, time to first tool call. |
| Does stored-response continuation preserve recent state? | `history-rehydration`. | Task correctness by turn, input-token growth, latency by turn. |
| How does concurrency affect capacity? | One workload with `SESSIONS` as a list, e.g. `1,5,10,25,50`, and `GATEWAY_PID` set. | Successful turns/s, p95/p99 latency, timeout and failure rates per level; the gateway's CPU and memory per level. |

`agentic-api` versus `vllm` is an end-to-end deployment comparison: it changes WebSocket versus HTTP/SSE and gateway
managed history versus client replay at the same time. Use `agentic-api` versus `agentic-api-http` to focus more
narrowly on transport within the gateway, or `agentic-api-http` versus `vllm` to compare the gateway and direct path
while holding HTTP/SSE constant.

### Transport workload

One transport turn is a deterministic sequential tool loop rather than one model request. With
`TRANSPORT_ROUNDS=4`, the sequence is:

```text
user prompt
  -> model function call benchmark_step(step=1)
  -> client-executed function call output
  -> model function call benchmark_step(step=2)
  -> client-executed function call output
  -> model function call benchmark_step(step=3)
  -> client-executed function call output
  -> model function call benchmark_step(step=4)
  -> client-executed function call output containing the final marker
  -> model repeats that marker as its final answer
```

The function runs in the benchmark process and returns a deterministic marker. It performs no network or application
work, so most measured time comes from inference, event delivery, function-call serialization, continuation, and
gateway state handling. The benchmark requires calls to use the expected run ID, step number, and total step count in
order. A turn is task-correct only if all calls match, the final function call output contains the expected marker,
and the model's final answer is exactly that marker. Parallel tool calls are disabled; this workload intentionally
measures sequential continuation.

This workload is useful for measuring:

- the steady-state cost of reusing a transport across repeated independent turns;
- the accumulated cost of a multi-round tool loop;
- whether function calls, function call outputs, and call IDs survive continuation correctly;
- whether gateway storage and rehydration work between inference rounds within one turn;
- throughput and tail latency when many tool loops execute concurrently.

The primary metrics are `end_to_end_latency_ms`, `initial_model_round_ms`,
`continuation_round_latencies_ms`, `time_to_first_tool_call_ms`, task correctness, and successful turns/s. Compare
multiple `TRANSPORT_ROUNDS` values to estimate how latency grows per continuation round. `mean_tool_duration_ms`
measures the time between the first and completed streaming events for a function call; it does not measure a real
external tool's execution time. This synthetic workload therefore does not predict the latency of database, web, or
other production tools. Connection establishment happens before the start barrier and is excluded, so this workload
also does not measure WebSocket or HTTP connection setup time.

### Tool-call workload

Each tool-call turn selects one deterministic row from the Berkeley Function-Calling Leaderboard (BFCL) question
file and joins it to the possible-answer file by case ID. The benchmark sends the row's user request and function
schemas as native Responses function tools, then evaluates the response's `function_call` output items. Selection is
the contiguous range starting at `DATASET_OFFSET`; `SEED` does not shuffle BFCL rows. The required dataset size is:

```text
SESSIONS * REQUESTS_PER_SESSION + DATASET_OFFSET
```

Every BFCL case is independent, conversation state is reset after it, and the workload performs one inference round.
It does not execute the selected function and does not submit a function call output. A call is correct only when the
number of calls and function names match, no unexpected argument keys are present, and each argument equals one of
the accepted top-level BFCL values. Call order is ignored. An empty-string possible answer allows an optional argument
to be omitted.

Use `simple_python` to test argument construction with one available function. Use `multiple` or `live_multiple` to
test selection of one function from several candidates. `multiple` does not mean parallel function calling. The BFCL
workload currently keeps `parallel_tool_calls=false`, so do not use BFCL `parallel`, `parallel_multiple`,
`live_parallel`, or `live_parallel_multiple` categories.

The primary metric is `tool_call_accuracy`. `success_rate` is stricter because it additionally requires a completed
response with no timeout or provider error. `time_to_first_tool_call_ms` is the best streaming responsiveness metric;
`ttft_ms` may be absent when a model emits a function call without output text. Run each BFCL category separately to
distinguish schema/argument accuracy from function-selection accuracy.

This is a lightweight BFCL-compatible comparison, not the complete official BFCL evaluator. It performs exact
top-level accepted-value matching, does not recursively interpret every nested BFCL alternative, and does not run
BFCL executable cases. Use it to compare identical gateway and direct-vLLM traffic, but do not report its aggregate
as an official BFCL leaderboard score.

### History-rehydration workload

This workload creates a synthetic chain of secrets unique to each session. Turn 0 tells the model to output marker 0
and privately remember marker 1. Every later turn intentionally omits the previous secret, asks the model to recall
it exactly, and supplies a new secret for the following turn:

```text
turn 0 input: output M0; remember M1       expected output: M0
turn 1 input: recall the previous secret; remember M2
                                              expected output: M1
turn 2 input: recall the previous secret; remember M3
                                              expected output: M2
```

The gateway arm sends `store: true`; after the first turn it sends only the new input plus the previous response ID.
The gateway must load the stored item history, preserve its ordering, and provide it to inference. The direct-vLLM
arm sends `store: false` and replays all accumulated input and output items on every request. Both arms ask the same
semantic question, but their request sizes and state-management responsibilities intentionally differ.

A turn is correct only when the final answer, after trimming surrounding whitespace or backticks, exactly equals the
hidden marker. This catches missing history, an incorrect previous response ID, reordered items, cross-session state
leakage, and state that was stored but not included in later inference. It is a narrow synthetic recall check: it does
not evaluate summarization, real conversational coherence, tools, compaction, or recall from far back in the session.

Task correctness and success rate are the primary correctness metrics. Examine `turns.csv` by `turn` for
`end_to_end_latency_ms`, first-output latency, TTFT, input tokens, and `request_bytes`/`response_bytes`. Model-visible
input tokens should grow for both arms because the gateway rehydrates the history before inference; only the
client-to-gateway request stays small for the gateway arm. `request_bytes` is the direct measurement of that
difference: flat across turn depth for the gateway arm, growing for direct vLLM since its client must resend the
full accumulated history every turn. Because every turn has one model response, `initial_model_round_ms` is useful
but continuation-round latency is empty.

## Run

The optimized wrapper saves the exact command, live events, diagnostics, and reports in a timestamped directory:

```bash
MODEL="Qwen/Qwen3.5-35B-A3B-FP8" \
WORKLOAD=transport \
SESSIONS=10 \
REQUESTS_PER_SESSION=10 \
TRANSPORT_ROUNDS=4 \
PROVIDER=both \
./scripts/run-response-provider-benchmark.sh
```

`PROVIDER=both` means Agentic API WebSocket versus direct vLLM HTTP/SSE. `PROVIDER=all` runs all five transport/provider
combinations. Individual values are:

```text
agentic-api       Agentic API WebSocket streaming
agentic-api-http  Agentic API HTTP/SSE streaming
agentic-api-json  Agentic API HTTP/JSON non-streaming
vllm              direct vLLM HTTP/SSE streaming
vllm-json         direct vLLM HTTP/JSON non-streaming
```

To run the gateway first, restart vLLM, and then run the direct arm with identical generated prompts:

```bash
MODEL="Qwen/Qwen3.5-35B-A3B-FP8" WORKLOAD=transport \
PROVIDER=agentic-api SEED=20260817 ./scripts/run-response-provider-benchmark.sh

# Restart vLLM here.

MODEL="Qwen/Qwen3.5-35B-A3B-FP8" WORKLOAD=transport \
PROVIDER=vllm SEED=20260817 ./scripts/run-response-provider-benchmark.sh
```

For the equivalent JSON comparison, use `PROVIDER=agentic-api-json` and `PROVIDER=vllm-json`.

The direct Cargo command is:

```bash
cargo bench -p agentic-server --bench response_provider -- \
  --model "Qwen/Qwen3.5-35B-A3B-FP8" \
  --workload transport \
  --provider both \
  --sessions 10 \
  --requests-per-session 10 \
  --transport-rounds 4 \
  --live-jsonl
```

No Codex model catalog or `CODEX_HOME` is needed because the benchmark sends Responses requests directly.

## BFCL tool-call workload

Use an official BFCL checkout and select a v4 category:

```bash
MODEL="Qwen/Qwen3.5-35B-A3B-FP8" \
WORKLOAD=tool-call \
PROVIDER=both \
BFCL_ROOT=/path/to/BFCL \
BFCL_CATEGORY=simple_python \
SESSIONS=10 \
REQUESTS_PER_SESSION=10 \
./scripts/run-response-provider-benchmark.sh
```

The wrapper derives these files:

```text
berkeley-function-call-leaderboard/bfcl_eval/data/BFCL_v4_<category>.json
berkeley-function-call-leaderboard/bfcl_eval/data/possible_answer/BFCL_v4_<category>.json
```

You can instead set `DATASET_QUESTIONS` and `DATASET_ANSWERS` explicitly. `DATASET_OFFSET` chooses the first case.
Cases are assigned deterministically and are paired across providers. Set `PRINT_PROMPTS=1` to validate the dataset
and print the generated cases without contacting either provider.

## Results

Each run directory contains:

```text
command.sh
live-events.jsonl
benchmark.log
run.json
turns.csv
summary.md
events/<provider>/session-NNN/turn-NNN.responses.jsonl
events/<provider>/session-NNN/turn-NNN.timestamped.jsonl
events/<provider>/session-NNN/turn-NNN.errors.log
```

`success` is strict: the request must reach a completed Responses terminal event without a timeout or provider error,
all locally handled transport tools must succeed, and the workload-specific correctness check must pass. A BFCL turn
is correct only when the function names, number of calls, and accepted arguments match its ground truth. A history turn
is correct only when the final answer is exactly the expected hidden marker.

Useful metrics are:

| Metric | Meaning |
| --- | --- |
| `end_to_end_latency_ms` | Request send through the logical turn's final terminal event, including all tool continuations. |
| `initial_model_round_ms` | Latency of the first model response in a logical turn. |
| `continuation_round_latencies_ms` | Latency of each response after a local function result is submitted. |
| `time_to_first_output_event_ms` | Streaming request send to the first output/reasoning/function event. |
| `ttft_ms` | Streaming request send to the first `response.output_text.delta`. It is null for HTTP/JSON. |
| `time_to_first_tool_call_ms` | Streaming request send to the first function-call event. |
| `request_bytes` | Serialized request-body bytes sent to the provider for the whole logical turn. |
| `response_bytes` | Raw response payload bytes (WS text frames / SSE chunks / JSON body) received for the turn. |
| `tool_call_accuracy` | Fraction of planned turns whose function names and arguments match the expected calls. |
| `task_correctness_rate` | Fraction of planned turns passing the workload-specific semantic check. |
| `success_rate` | Fraction passing transport, completion, execution, and semantic checks together. |
| `successful_turns_per_second` | Successful turns divided by the slowest concurrent session wall time. |

`p50` is the median sample. `p95` is the value at or below which approximately 95% of samples fall; it highlights tail
latency. `p99` targets rarer tail behavior and needs substantially more samples before it is stable. Compare latency
only between successful turns, while also reporting correctness and failure rates so faster failures are never counted
as wins.
