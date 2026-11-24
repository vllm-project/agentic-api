# Async client tool recordings

These recordings characterize [async tool calling](https://developers.openai.com/api/docs/guides/async-tool-calling)
for issue [#332](https://github.com/vllm-project/agentic-api/issues/332). A function or custom tool declared with
`async: true` lets the model keep working after it emits the call; the client returns the result later on the
original `call_id`.

There are two sets:

- **OpenAI reference** (`gpt-6-astra`): the public contract the gateway must match.
- **Gateway** (`Qwen/Qwen3.6-35B-A3B` behind the gateway): the OpenAI reference scenarios replayed through the
  gateway, to compare its public behavior with OpenAI's. An OpenAI model keeps answering after an async call; a
  model served by vLLM stops at the call, so the gateway runs another round itself with the call still pending and
  two hints added (see [How the hints were chosen](#how-the-hints-were-chosen)).

Each scenario records only the modes that answer a distinct question; streaming is recorded where event order matters.

## Files

| Recording | Mode | Requests | What it shows |
|---|---|---|---|
| **OpenAI reference** | | | |
| `function-delayed-result` | both | 4 | An async call with an answer in the same response; two follow-ups without the output; the late output on the original `call_id`. Streaming: the call completes before later items start |
| `custom-delayed-result` | non-streaming | 3 | The same contract for `custom_tool_call` / `custom_tool_call_output` |
| `web-search-mixed` | streaming | 2 | An async call and a hosted `web_search_call` in one response; the async call completes before the search starts |
| `wait-tool` | non-streaming | 2 | Two async lookups and an application-defined synchronous `wait_for_tasks`; results are delivered before the wait status |
| `parallel-mixed` | non-streaming | 3 | Parallel async and synchronous calls; answering only the synchronous one is accepted, the async one is answered later |
| `edge-cases` | non-streaming | 11 | Probes: unanswered synchronous call, unknown `call_id`, duplicate and conflicting outputs, re-declaring the tool without `async`, an unsupported model, and `async` on a hosted tool. The async probes are branches of one response with the async call (`async-call/t1-start`) |
| `client-tool-types` | non-streaming | 6 | `async` on a namespace member function, on the namespace itself, on the client `shell` tool, and on client `tool_search` |
| `multi-agent-parallel` | non-streaming | 1 | Multi-agent mode with `parallel_tool_calls: true` and an async tool |
| `multi-agent-sequential` | non-streaming | 2 | Multi-agent mode with parallel calls off; the async call stays pending until a late output |
| **Gateway** | | | |
| every OpenAI scenario except `web-search-mixed` | as above | as above | The same steps through the gateway; `web-search-mixed` needs a configured web-search provider and is not recorded |

File names are `async-tool-<provider>-<scenario>-<model>-<mode>.yaml`. `tools.json` holds the tool declarations and
`prompts.json` the prompts, tool outputs, and the gateway's upstream hint texts (`upstream_hints`).
`async_tool_cassette_test.rs` checks that the directory holds exactly this set.

## How they are recorded

`../record_async_tool_cassettes.py` drives scripted scenarios through the shared `record_cassette.py` proxy (it imports
`_start_proxy`/`_stop_proxy`, like `record_messages_tool_choice.py`). Scripting is needed because outputs are held back
across follow-ups and some probes are expected to fail. Each request carries an `x-run-id: <scenario>/<step>` header,
which the proxy records, so tests find steps by label. A probe's HTTP status is recorded, not required; a step that
later steps depend on (for example, the model making the async call) aborts the recording, which is kept as
`*.failed.yaml` for inspection. A recorder proxy failure such as a read timeout also aborts, so it is never mistaken
for an upstream response. Recordings are staged and renamed only after they succeed, and refused if authorization
was not masked.

```bash
# OpenAI reference (OPENAI_API_KEY exported)
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  python crates/agentic-server-core/tests/cassettes/record_async_tool_cassettes.py

# Hinted vs unhinted continuation outcome rates against a model server (nothing recorded;
# VLLM_API_KEY is sent as a bearer token when set)
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  python crates/agentic-server-core/tests/cassettes/record_async_tool_cassettes.py \
  --provider vllm --base-url http://localhost:8000 --model Qwen/Qwen3.6-35B-A3B --sample 24

# Gateway (start it first: cargo run -p agentic-server -- --llm-api-base http://localhost:8000)
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  python crates/agentic-server-core/tests/cassettes/record_async_tool_cassettes.py \
  --provider gateway --base-url http://localhost:9000 --model Qwen/Qwen3.6-35B-A3B --scenario edge-cases

cargo test -p agentic-server-core --test async_tool_cassette_test
```

Use `--scenario <name>` to record one scenario, `--dry-run` to list file names, `--stream-mode` to override a
scenario's modes, and `--output-dir` to record somewhere other than this directory. Gateway recordings and samples use
`max_output_tokens: 16384`; OpenAI recordings use 4096. For the gateway, the unsupported-model probe uses the served
model, because the gateway accepts `async` for every model.

Recordings against an open model can abort when the model skips a step that later steps depend on: it sometimes
answers, and even says the lookup has started, without making the async call. That is model variability, not
gateway behavior: re-run the scenario. `edge-cases` makes the async call once and branches every probe from that
response, so it depends on the call only once. A recording that aborts is kept as `*.failed.yaml`; delete it before
re-running, since the tests require the directory to hold exactly the declared set.

## What the OpenAI reference established

Recorded 2026-10-01 and 2026-10-02 against `gpt-6-astra` (`edge-cases` re-recorded 2026-10-08); `gpt-5.5` is the
unsupported-model probe.

- Continuations chained by `previous_response_id` are accepted while an async call has no output, and a later output
  on the original `call_id` is accepted. A synchronous call left unanswered is still rejected with 400,
  `param: input`, `No tool output found for function call <id>.`
- When the model makes async calls only, it keeps answering in the same response. When a synchronous client call
  comes with them (`parallel-mixed`, `wait-tool`), the response ends after the calls, with no message.
- An output for an unknown `call_id` is rejected with 400, `param: input`,
  `No tool call found for function call output with call_id <id>.` Duplicate outputs (in one request or across two)
  and two different outputs for one call are accepted, and the model receives all of them: with two different
  outputs in one request, the answer reported the second as "the latest" snapshot.
- Re-declaring the tool without `async` while a call is pending is accepted: the pending call is not treated as an
  unanswered synchronous call.
- `async` is accepted on a namespace member function; its call carries `async: true` and `namespace`, and follows the
  same pending contract. `async` on the namespace itself, on the client `shell` tool, on client `tool_search`, and on
  hosted `web_search` returns 400, `code: unknown_parameter`, `param: tools[0].async`.
- `async` on a model without support returns 400, `code: unsupported_value`, `param: tools`,
  `Async tools are not supported with gpt-5.5.`
- Multi-agent mode with `parallel_tool_calls: true` returns 400, `code: unsupported_value`, `param: multi_agent`,
  `Async parallel tool calls are not yet supported in multi-agent mode.` With parallel calls off it is accepted. In
  that response, with the async output still pending, the root agent called the hosted `wait_agent` action on itself
  (`/root`, `timeout_ms: 10000`) two or three times; each returned `"timed_out": true` before the response completed.
  The multi-agent scenarios ask the root agent not to start subagents; without that, one request outlived the
  recorder proxy's default 300-second read timeout (the driver now allows 900 seconds).
- When streaming, the async call's `response.output_item.done` arrives before any later output item starts, so a
  client can dispatch it while consuming the rest of the response.

## How the hints were chosen

On 2026-10-02, requests sent straight to vLLM serving `Qwen/Qwen3.6-35B-A3B` (`max_output_tokens: 16384`; at 4096, one
late-output turn spent the whole budget deliberating over wording) showed why the gateway emulates async tools
rather than passing them through. Those recordings are no longer kept; the gateway set covers the same requests end
to end.

- `async: true` passed through unchanged is ignored: no error, and no call carries an `async` marker.
- The model server accepts the requests the gateway sends while a call is pending: `store: false`, the full item
  history (every output item replayed, reasoning included), `async` stripped, and a history with an unanswered call, a
  follow-up, and an output placed after later turns all return 200, and the model uses the late output.
- Without hints the continuation round is unreliable. With the two `upstream_hints` texts (a suffix on the async
  tool's description, and a `developer` message right after each call that has no output in that request) it is not.

`--sample` measures this: it runs unrecorded first and continuation rounds and classifies the continuation: `ok`,
`re-called` (it called a tool again), `fabricated` (it stated a temperature), `no-call` (no call in the first round),
or `error`. On 2026-10-02, 24 samples per mode:

| Mode | ok | re-called | fabricated | no-call | error |
|---|---|---|---|---|---|
| unhinted | 4 | 1 | 14 | 5 | 0 |
| hinted | 18 | 0 | 0 | 6 | 0 |

The hint does not reduce how often the first call happens (`no-call` is similar in both modes). The exact hint texts
and their placement are pinned by unit tests in `src/tool/async_execution.rs`.

## What the gateway recordings established

Recorded 2026-10-08 through the gateway against `Qwen/Qwen3.6-35B-A3B` served by an OpenAI-compatible router. Every
step has the status OpenAI returned for the same step, and every error has OpenAI's type, code, param, and message
(call IDs aside). The one deliberate difference is the unsupported-model probe: the gateway accepts `async` for every
model it serves.

- An async call and the answer arrive in one response, as with OpenAI, because the gateway runs the continuation
  round with the hints. Streaming completes the async call, marked `async: true`, before the answer starts.
- A synchronous call next to async calls (`parallel-mixed`, `wait-tool`) ends the response after the calls.
- Multi-agent mode rejects `parallel_tool_calls: true` with an async tool and accepts it with parallel calls off.
- No hint text appears in a gateway output item or in the echoed `tools`. The model's own reasoning sometimes quotes
  or paraphrases the hints, including a namespace member's flattened model-visible name; the gateway does not filter
  reasoning.

`async_tool_cassette_test.rs` compares the two sets step by step.
