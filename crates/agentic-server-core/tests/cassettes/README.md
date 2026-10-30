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

`record_responses_api_cassettes.sh` extends the existing recorder's `items` mode.
Each provider gets one flat `turns` list per suite, using the same capture format as
`cassettes/conversations`. Individual edge probes record the observed status rather
than assuming that OpenAI and the gateway accept the same requests.

| `RESPONSES_SCENARIO` | HTTP turns | Coverage |
| --- | --- | --- |
| `lifecycle` | 19 | Stored parent/child responses, input pages, retrieval, parent deletion, child visibility, continued inference after deletion, repeated deletion. |
| `pagination` | 16 | Ascending, descending and default order; one-item pages; limit 100; cursors at the end; a cursor from an unrelated response. |
| `edge-cases` | 23 | Unstored/missing IDs; missing parent; invalid, repeated and out-of-range query parameters; unknown and repeated includes; repeated deletion. |
| `conversation` | 19 | Responses attached to a conversation, manual item insertion/deletion, snapshots before and after mutations, response deletion, conversation deletion. |
| `replayed-items` | 14 | Generated-ID reuse, two copies of an ID with different content, wrong-prefix IDs, function call/output history without public item IDs, seed immutability after probes. |
| `tool-items` | 48 plus additional pages | Replay existing parallel builtin/client/mixed tools, shell, code interpreter, tool search, and the multi-agent review task; inspect complete input pages after each response and a final continuation. |

From the worktree root, with an inference server already running:

```bash
# One-time recorder dependencies.
uv venv .venv
uv pip install --python .venv/bin/python -r crates/agentic-server-core/tests/cassettes/recorder-requirements.txt

# Start the release gateway in a separate terminal; persistence must be enabled.
AGENTIC_API_HOME=/home/akk/maral/res/.agentic-api \
  ./target/release/agentic-server --llm-api-base http://127.0.0.1:5050 --gateway-port 9000

# Load credentials without printing them. Record each provider independently.
set -a; source /home/akk/maral/res/.env; set +a
RESPONSES_RECORD_SET=openai OPENAI_MODEL=gpt-4.1 \
  bash crates/agentic-server-core/tests/cassettes/record_responses_api_cassettes.sh
RESPONSES_RECORD_SET=gateway GATEWAY_MODEL=Qwen/Qwen3.8-27B-FP8 GATEWAY_URL=http://127.0.0.1:9000 \
  bash crates/agentic-server-core/tests/cassettes/record_responses_api_cassettes.sh

# Replay both providers' recordings through the actual gateway handlers.
cargo build --locked -p agentic-server --bin agentic-server
cargo test --locked -p agentic-server-core --test responses_api_test
```

Use the exact model alias served by your inference engine for `GATEWAY_MODEL`.
`RESPONSES_SCENARIO=all` is the default; select any row above to replace only that suite.
`RESPONSES_STREAM_MODE=nonstreaming` is the default. Set `streaming` or `both` to
exercise streamed response creation as well; management calls still return JSON.
The `replayed-items` suite keeps admission probes non-streaming so rejected request
bodies remain inspectable, while its seed response follows the selected mode.
Files are named `responses/response-<scenario>-<provider>.yaml`, with `-stream`
appended to the scenario for streaming runs. Incomplete recordings remain `.yaml.tmp`
and do not replace a previously validated capture.

The `tool-items` suite uses `replay_old_cassettes.py` through the shared recorder.
It replays the original tool-enabled requests from the non-streaming OpenAI
`parallel_tool_calls/` builtin-only, client-only and mixed suites, plus `shell/`
and `code_interpreter/`, `tool_search/`, and only the `multi_agent/` review case. MCP uses the public documentation-search server declared
in the parallel fixture. Client outputs use the existing simulated tool callbacks
with the live response's calls and IDs; no shell commands are executed.

Each successful creation is followed by `GET /responses/{id}/input_items`,
following `has_more` and `last_id` until the complete input history is captured.
A final continuation resolves pending client calls and makes the preceding output
visible in the next input-items page. Rejected creations are recorded and end that
scenario, so configuration failures can produce fewer than 48 HTTP turns. Pagination
can produce additional GET requests even with `limit=100`. These
captures use JSON even when both transports are selected. To refresh them:

```bash
RESPONSES_RECORD_SET=all RESPONSES_SCENARIO=tool-items OPENAI_MODEL=gpt-5.6 \
  GATEWAY_MODEL=Qwen/Qwen3.8-27B-FP8 \
  bash crates/agentic-server-core/tests/cassettes/record_responses_api_cassettes.sh
```

For the review case, `OPENAI_MULTI_AGENT_MODEL` defaults to `gpt-5.6-sol`; the
recorder preserves the fixture's multi-agent beta header. Gateway cases always use
`GATEWAY_MODEL`. Tool-search continuations reuse the fixture's discovered definitions
and client output callbacks through the shared recorder.

The gateway needs MCP access to the fixture's server and an enabled code-interpreter
runtime to complete those cases. Recorded failures do not establish tool parity.

`recorded_tool_execution_ids_and_statuses` checks both provider captures, including
complete pagination, tool item ID prefixes and statuses, and parent output identity
in continuation input pages. The live gateway replays cover the five
management suites plus client-tool and tool-search input pages from the gateway execution capture.
Those tool replays assert result IDs and completed statuses against the OpenAI
contract even when the older gateway capture lacks those fields. OpenAI's opaque
reasoning cannot serve as vLLM inference history, so the live client-tool replay
uses the gateway's own recorded reasoning without dropping it. Separate web-search
and MCP execution regressions verify that stored tool call/result pairs remain in
both inference history and input-item listings.

The refreshed gateway capture contains 48 successful HTTP exchanges, including code
interpreter, tool search, and the multi-agent review case. All fifteen tool and
orchestration item types match the OpenAI reference's ID-prefix and status contracts;
parent output identities are preserved in continuation input pages. The Rust replay
executes the client function/custom, shell, and tool-search flows (13 Responses
requests plus their input pages) through a local gateway with inference mocked.
This validates the recorded successful flows, not every possible tool failure.

`HTTP_READ_TIMEOUT` overrides the recorder's upstream read timeout. Tool-item
recordings default to 900 seconds, matching the existing multi-agent recorder;
other management scenarios default to 300 seconds.

`RESPONSES_CASSETTES_DIR` overrides the output/replay directory and
`RECORDER_PROXY_PORT` overrides the local proxy port (default 7070).
Repeated query values are retained as arrays in `request.query_params`; a single
value remains a string. No observed response body or status is rewritten.

The Rust tests require the five original suites for both providers in JSON and streaming mode.
They assert the recordings directly, then replay every management request through
a local gateway process with only inference mocked. `RESPONSES_GATEWAY_BIN` can
select an explicitly built binary; otherwise the tests use `agentic-server` from
the Cargo profile directory. Error `type`, `code`, and `param` are checked against
the OpenAI reference. Log probabilities are excluded from item comparisons.
The lifecycle suite also checks that the continuation retains the surviving child's
history and new user message after parent deletion, and inspects the live gateway's
upstream request to ensure the deleted parent's user message is not sent to inference.
Client-supplied function-call status defaults remain deferred: OpenAI lists an
omitted call status as `in_progress`; these comparisons tolerate its absence while
checking other values. Function, custom, and shell result types retain their IDs
and statuses. Stored results use `fc_`, `ctco_`, and `sho_` IDs respectively.
Request input conversion fills omitted result statuses as `completed` before
persistence, without changing explicit statuses or call execution states.
Gateway-created function results receive their identity and completed status at
creation. Input-item listings return stored items without classifying gateway call/result
pairs by their projected IDs. The gateway tool capture predates removal of that
filter, so its built-in-tool pages must be re-recorded before claiming parity for
this listing behavior. The captured YAML is retained unchanged.
Stateless creation remains upstream pass-through on this branch, so its optional
`store` response field is not compared across providers; subsequent CRUD calls
verify that the response was not retained. Differences in model-generated output
are checked within each provider's pages, while client input must match across providers.

Reference: [Responses input items](https://developers.openai.com/api/reference/resources/responses/subresources/input_items/methods/list)
and [Conversation items](https://developers.openai.com/api/reference/resources/conversations/subresources/items/methods/list).

## Mid-turn steering exploration

`record_multi_turn_steering_cassettes.sh` records OpenAI references or gateway observations of the WebSocket
`response.steer` operation through
`websocket_recorder.py steering`, using its existing duplex socket/session recorder. HTTP steering probes, modes
and cassettes have been removed. Ordinary HTTP recording remains in `record_cassette.py`.

The shared `multi_turn_steering/fixtures.py` supplies prompts, category membership, media and tool fixtures. It reuses
`record_cassette.py`'s ordinary tool loaders/output builder through the WebSocket driver. Transport logic stays in
the main recorder. All prompts are documented in
[multi_turn_steering/prompts.txt](multi_turn_steering/prompts.txt). No findings JSON or extra recorder test files are generated.

Six categories produce **six YAML files per provider** under `multi_turn_steering/`.
Image steering is separate so the other five categories can use a text model:

| Category / filename scenario | Coverage | Selection |
| --- | --- | --- |
| `mixed-tools-steer` | Original [parallel mixed-tools](parallel_tool_calls/parallel-mixed-openai-reference-gpt-5.6-nonstreaming.yaml) workflow; web search, weather function and custom temperature-unit call, with two steering updates | `--suite smoke` (default) |
| `client-tools-steer` | Client-only search, shell, custom and discovered function calls; steer each completed call batch before returning its outputs | `--suite client-tools` |
| `edge-input-lifecycle` | 18 cases: input validation, repeated steering, timing, completed targets, old-parent admission timing, store:false and two ordered user messages in one steer | `--suite input-lifecycle` |
| `edge-tools-settings` | 13 cases: pending/withheld outputs, wrong/partial-output recovery, running hosted tools, queue pressure, automatic/explicit/default settings and store:false recovery | `--suite tools-settings` |
| `edge-connections` | Three cases: old target after reconnect, disconnect with pending steering, and parent/lane matching | `--suite connections` |
| `vision-steering` | One case: `array-image-user`, using the existing red/blue PNG in a steering message | `--suite vision` |

`--suite non-vision` selects the five text/tool categories. `--suite vision` selects only image steering.
`--suite edges` selects the three non-vision edge categories; `--suite all` selects all six categories.
`--case edge-input-lifecycle`, `edge-tools-settings`, `edge-connections`, or `vision-steering` selects that category.
`--case edge-cases` remains an umbrella alias selecting the three edge files. An individual `--case NAME`
records only that scenario in its category filename, replacing that category's previous capture.

Run from the repository root with `OPENAI_API_KEY` exported:

```bash
# Inspect cases and output paths; no requests or file writes.
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_turn_steering_cassettes.sh --suite all --dry-run

# Record all six categories, including the separate vision-steering file.
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_turn_steering_cassettes.sh --suite all

# Record only tool/output/settings edge cases.
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_turn_steering_cassettes.sh --suite tools-settings
```

To record the running gateway, use its served model name:

```bash
# Serve Qwen3.6, then record the five text/tool categories.
GATEWAY_URL=http://localhost:9000 GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 \
  uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_turn_steering_cassettes.sh \
  --record-set gateway --suite non-vision

# Switch the backend to Qwen-VL, then record only the image category.
GATEWAY_URL=http://localhost:9000 GATEWAY_MODEL=Qwen/Qwen3-VL-30B-A3B-Instruct \
  uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_turn_steering_cassettes.sh \
  --record-set gateway --suite vision
```

Add `--dry-run` to inspect the selected gateway output paths without contacting it. The recorder does not start the
server. `--record-set openai|gateway|all` defaults to `STEERING_RECORD_SET=openai`; `all` records both providers.
`--gateway-url` overrides `GATEWAY_URL` (default `http://localhost:9000`, converted to `ws://localhost:9000/v1/responses`).
Gateway recording requires `GATEWAY_MODEL`, its `MODEL` fallback, or `--model`. The optional `GATEWAY_API_KEY` supplies
only gateway authentication; the recorder never falls back to `OPENAI_API_KEY` for gateway requests. Credentials
are masked in captures and never printed. Supply credentials through these environment variables, not URL userinfo.

The OpenAI default model is `gpt-6-sol`; override with `OPENAI_MODEL` or `--model`. Only its `unsupported-model`
probe uses GPT-5.6. Gateway runs explicitly skip that probe because OpenAI model availability restrictions are not
modeled locally; all other scenarios retain the same prompts and fixture outputs. A gateway may lack a model/tool
capability needed by a probe; its actual failure or missing trigger remains visible rather than becoming a parity pass.
`--model` overrides the model for every selected provider; use separate model environment variables with `--record-set all`.
`--timeout` defaults to 300 seconds per scenario, including both sockets for reconnect cases; close cleanup has
its own maximum five-second budget. The default per-response output cap is 8192 tokens. Live calls incur usage.
`--repeat` labels fresh attempts inside the same category file. Reruns replace selected category files.
The vision-only command leaves all five text/tool category files untouched; `--suite non-vision` leaves
the vision file untouched. `--case array-image-user` writes the same separate vision filename.
Use `--help` or `--list-cases` for selection. HTTP transport/stream-mode/timeout flags are absent from this wrapper.

YAML uses `format: responses-websocket-v1`; each session preserves actual handshake headers, ordered client/server
frames and close outcomes. Stable filenames follow
`multi-turn-steering-openai-reference-CATEGORY-MODEL-websocket.yaml` or
`multi-turn-steering-gateway-CATEGORY-MODEL-websocket.yaml` (model names are sanitized for filenames).
Gateway runs preserve reference filenames. No simulated provider events are written.
Tool outputs come from existing local fixtures; generated commands are never executed by the recorder.

The `client-tools-steer` OpenAI and Qwen3.6 gateway captures both complete successfully.
Select it alone with `--suite client-tools`. It uses the existing multi-agent client tool
catalog, local shell declaration and simulated output handlers in a **single-agent** request. It discovers
Paris weather/time-zone tools, requests shell and custom calls, then uses the discovered functions.
After each completed response with client calls, it withholds outputs, submits a marked steering update,
waits for pending obligations and returns the actual matching outputs. A ten-continuation budget bounds
the workflow. Missing any of the four pending output kinds is inconclusive; mismatched call IDs or kinds
are reported separately. Raw frames retain names, execution fields, markers and final answers for parity
review. The OpenAI capture completes six responses and accepts five steering updates, each followed by
pending obligations for search, shell, custom, weather or time-zone output. All output types and call IDs
match the withheld calls. The final assistant answer includes the actual fixture results and all five
`CLIENT_STEER_001` through `CLIENT_STEER_005` markers, with no rejected or incomplete responses.
Qwen groups weather and time zone in one response, despite the shared `parallel_tool_calls:false` setting,
so its workflow has five responses and four steering updates. Its final assistant answer includes all
four submitted markers and the same fixture results. Both providers preserve the five client obligations;
batch sizes and raw reasoning events differ. The batching difference is not a steering-control mismatch.
The `client_tools_captures_compare_obligations_and_drive_real_handlers` integration test compares the
shared initial settings, pending output kinds/names, call-ID linkage, withheld-output ordering and final
results. It replays both complete client graphs through current gateway handlers and asserts that accepted
steering reaches successor inference exactly once and in order. Mock upstream settings follow tool
discovery, so deferred declarations are checked before and after discovery.

```bash
cargo test --offline -p agentic-server --test responses_websocket_test client_tools_captures_
```

The current OpenAI references include the following additional probes:

- `pending-partial-outputs`: submit one of two required outputs, then recover with all saved outputs.
- `pending-output-recovery-store-false`: characterize whether an invalid continuation loses transient parent state.
- `settings-explicit-defaults`: omit inherited settings in an explicit continuation and observe defaults.
- `lane-parent-mismatch`: provide outputs on another lane, observe that branch/rejection, then continue on the owning lane.
- `array-image-user`: attach the existing red/blue PNG to steering input.
- `array-multiple-user`: submit two ordered user messages in one steering event, with distinct data and verification tokens.

Reviewed observations establish wrong-output recovery without resending steering, old-parent acceptance before
successor creation and rejection after it, automatic settings inheritance, explicit replacement/default settings,
and connection-local targets and pending input. A capture rejection is evidence; timeout, missing prerequisites and transport failures remain
inconclusive. Acceptance alone does not establish application. Media probes require reviewing the actual answer.
The settings probes compare fields/format/markers; they do not exhaust token or built-in-tool budgets.
`max_tool_calls` does not limit the status function used by these fixtures.
The separate vision category is recorded for OpenAI and Qwen-VL. Both captured client graphs replay through the
real gateway handlers, checking exact image/detail forwarding and accepted input once at successor inference.
The Qwen-VL successor contains its description/marker in reasoning only, without an assistant message; this is
an explicitly qualified backend-output gap, not successful assistant-answer parity.

Tool scenarios now use `tool_choice: auto`, including the partial-output and hosted-tool cases.
They still require actual observed calls; missing calls remain inconclusive. Minimal explicit-default
continuations intentionally omit tool choice to check normal defaults. Older recordings retain their
original required/named choices. The current OpenAI and Qwen3.6 recordings both use the updated auto-choice scenarios.

The current Qwen3.6 tools/settings file contains all 13 cases. Three remain limited by upstream generation:
`settings-automatic` creates an inherited successor that exhausts 2,048 tokens without a final answer;
`pending-user-steer` exhausts 8,192 tokens before sending steering; and `settings-explicit-defaults`
exhausts its 2,048-token parent budget without the required call, so it sends no steering or continuation.
Tests assert these exact incomplete outcomes, then replay the complete OpenAI workflows through the real
gateway as additional protocol coverage. They do not classify these live recordings as completed parity.
The fixtures do not enable `context_management`; automatic compaction would reduce earlier input history,
not extend the output budget of an active generation.

The gateway recordings are compared with the OpenAI recordings in the existing
`agentic-server/tests/responses_websocket_test.rs` integration target. Its `duplex_replay::steering_` tests
check both captured causal contracts and replay client workflows through the real gateway handlers, using
local synthetic model dependencies derived from captured output items. A companion core executor test,
`recorded_hosted_steering_drains_real_interpreter_before_successor`, exercises the recorded interpreter
workflow with a controlled provider. Neither test contacts OpenAI or edits the YAML files.
Synthetic inference exposes custom calls as normalized functions to exercise the actual translator.
Its control gates can let steering reach a completed client call before the captured native token-limit
terminal. That replay may complete the parent instead, but must preserve the same calls and pending
obligations. This exception applies only to that fixture race; other terminal statuses remain checked.

From the repository root, run the cassette regressions with:

```bash
cargo test --offline -p agentic-server --test responses_websocket_test duplex_replay::steering_
cargo test --offline -p agentic-server-core --lib recorded_hosted_steering_drains_real_interpreter_before_successor
```

Vision comparison/replay is separate and requires its gateway capture. After recording it, run:

```bash
cargo test --offline -p agentic-server --test responses_websocket_test duplex_replay::vision_
```

To run the entire WebSocket integration target before the vision recording is ready, use
`cargo test --offline -p agentic-server --test responses_websocket_test -- --skip vision_`.
The vision tests remain strict about missing or ambiguous captures; the non-vision tests do not skip missing peers.

The first gateway comparison exposed two defects: custom-tool pending stubs were incorrectly labeled as
function outputs, and explicit `tools: []` continuations inherited an incompatible forced parent selector.
The fixes use the parent tool metadata for public obligation types and default tool choice when a new tool
list is supplied. The healthy gateway rerecords confirm both fixes, including reconnect and lane branching.

The latest non-vision audit covers 36 gateway and 37 OpenAI sessions across 34 shared named cases.
Old-parent rejection now matches in both captures; earlier acceptance differences were reservation races.
Mixed-tool batching and interpreter runtime outcomes remain explicitly qualified. Wrong-output recovery,
partial-output recovery with two actual calls, `store:false` recovery, lane ownership, and reconnect
isolation have matching steering control outcomes. In the burst capture the gateway model prints only
the final token, but replay's upstream input spy verifies that all three accepted inputs reach successor
inference exactly once and in order. Actual final answer wording is not a protocol assertion.

All 33 ordinary gateway client graphs and three complete OpenAI intent graphs run through real WebSocket
handlers. The hosted interpreter case runs through the companion core executor test for both providers.
The full non-vision WebSocket target passes all 88 tests; the two separate vision tests await recording.
These results establish steering regression coverage while preserving the three generation/coverage gaps above.

File steering is outside this gateway's supported input capabilities. Its scenario and sessions were
removed from both providers' lifecycle cassettes, preserving every other recorded frame. The replacement
`array-multiple-user` is now recorded on both providers: both final answers preserve the codename,
deadline, and two verification tokens in order. Image steering has its own `vision-steering` category;
its current OpenAI capture is ready, and the gateway capture awaits the vision model.
Vision tests compare normalized image input and the final verification token.
Image steering requires a vision-capable model. When changing gateway models,
archive the old gateway category files outside this directory before recording the new set: replay
requires exactly one gateway cassette per category and rejects ambiguous model sets.
For the split, archive the previous Qwen-VL `mixed-tools-steer`, `edge-input-lifecycle`, and
`edge-connections` gateway files before recording those categories with Qwen3.6. Keep the current
Qwen3.6 tools/settings file until its rerecord replaces it. OpenAI `--suite all` replaces its old
lifecycle category and records image steering in `vision-steering`; use both gateway commands above
to produce the same five-category layout. Existing captured frames are not rewritten by this change.

Multi-agent steering cases and captures are removed. The
[OpenAI steering reference](https://developers.openai.com/api/reference/resources/responses/websocket-events#response.steer)
restricts steering to single-agent responses. Ordinary multi-agent execution/injection uses its separate recorder.

See the [implementation RFC](../../../../../../rfcs/agentic-api-multi-turn-steering-issue.md) for evidence, interfaces
and remaining qualification gates.

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

The Codex job also runs `scripts/codex_reasoning_smoke.py`, which replays the reasoning cassette for a served model
advertised as `["low", "high", "max"]` and for a model without configured levels. It asserts the `reasoning.effort` the
pinned client put on the upstream request: `low` with no explicit selection because `medium` is not advertised, `max`
when selected with `-c model_reasoning_effort=max`, and `medium` for the model that keeps the default levels.

## Messages web fetch cassettes

`messages/messages-web-fetch-Qwen-Qwen3-8B-{nonstreaming,streaming}.yaml` record the upstream turns of a request that
declared the native `web_fetch_20250910` tool, as the gateway rewrites it: `messages/web_fetch_tools.json` is the
function tool the gateway sends upstream and `messages/web_fetch_tool_outputs.json` is the `web_fetch_result` the
gateway produces for the page `tests/messages_web_fetch_cassette_test.rs` serves. The fed-back `tool_result` carries
`is_error: false`, as the gateway loop's does. Recorded against vLLM-Metal serving Qwen3-8B (4-bit) as `qwen3` with
`--enable-auto-tool-choice --tool-call-parser hermes` and thinking off:

```bash
printf 'Fetch http://127.0.0.1:18080/page/release-notes.html with web_fetch and reply with the verification token it contains, nothing else.\n' \
  | python record_cassette.py --mode messages --turns 2 --no-stream --vllm http://127.0.0.1:5050 --model qwen3 \
      --tools messages/web_fetch_tools.json --tool-outputs messages/web_fetch_tool_outputs.json \
      --output messages/messages-web-fetch-Qwen-Qwen3-8B-nonstreaming.yaml
```

The streaming cassette is the same command with `--stream`. The replay binds an ephemeral port, serves the recorded
responses with the origin re-based onto it (the streaming `tool_use` input is re-emitted as one `input_json_delta`,
since the recorded chunks split the URL), and compares the loop's complete upstream requests with the recording,
normalizing only the origin and `retrieved_at`.

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
--mode MODE            responses | messages | conv | items | isolation | mixed | store_true_then_store_false
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
--manual-item-replay   Replay accumulated items with store=false (HTTP --mode responses with --no-store and no
                       branches; direct-vLLM or gateway tool search, or any such recording)
--reasoning JSON       JSON object containing Responses reasoning settings
--input-file FILE       JSON string or item array for turn 1 of an HTTP Responses recording; later turns are prompted
--max-output-tokens N  max_output_tokens for Responses requests (default 1024; use 0 to omit)
--max-tool-calls JSON  max_tool_calls per linear Responses turn: an integer for every turn, or an array where null
                       omits the field on that turn (e.g. '[1, null]')
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

## Persistent multi-agent WebSocket sessions

The multi-agent recorder supports a persistent duplex capture with
`MULTI_AGENT_TRANSPORT=websocket`. It reuses the HTTP suite's exact prompts and
tool fixtures, records actual frames in both directions, injects client outputs
while reading response events, and retains handshake/close/failure evidence.
See [the multi-agent recording commands and session format](multi_agent/README.md#persistent-websocket-recordings).
These files use `sessions`, not the HTTP `turns` schema or synthesized SSE.

## Recorder scripts

| Script | Cassettes | Backend |
|--------|-----------|---------|
| `record_multi_agent_cassettes.sh` | Five delegated scenarios over HTTP JSON/SSE or persistent duplex WebSocket | OpenAI and gateway |
| `record_text_only_cassettes.sh` | 10 text-only cassettes (responses + conv modes, streaming + non-streaming) | OpenAI (`OPENAI_API_KEY`) |
| `record_store_false_cassettes.sh` | Two HTTP `store:false` cassettes per provider (JSON/SSE), each containing six named scenarios | OpenAI and gateway |
| `record_conversations_api_cassettes.sh` | 18 Conversation Items API cassettes: four history scenarios in both transports and one non-streaming edge-case sequence, each for both providers | OpenAI and gateway |
| `record_reasoning_cassettes.sh` | Matching explicit-reasoning cassettes (streaming + non-streaming) | gateway and OpenAI reference; optional direct vLLM |
| `record_tool_call_cassettes.sh` | 8 tool-call cassettes (4 tool_choice modes x streaming + non-streaming) | vLLM |
| `record_codex_cli_tool_call_cassettes.sh` | Codex function/namespace/custom-tool matrix | gateway, vLLM, and OpenAI |
| `record_custom_tool_cassettes.sh` | Matching two-turn custom-tool flows (streaming + non-streaming) | gateway and OpenAI reference |
| `record_async_tool_cassettes.py` | Async function/custom tool scenarios and probes; each scenario declares its modes | OpenAI reference and gateway (vLLM: unrecorded sampling) |
| `record_shell_cassettes.sh` | Four two-turn local-shell scenarios (streaming + non-streaming) | gateway and OpenAI reference |
| `record_mcp_cassettes.sh` | Native MCP counter tool discovery and calls (streaming + non-streaming) | gateway and OpenAI reference |
| `record_web_search_cassettes.sh` | Matching web-search calls (streaming + non-streaming) | gateway and OpenAI reference |
| `record_max_tool_calls_cassettes.sh` | `max_tool_calls` validation, per-tool exhaustion, call counting, and WebSocket scenarios | gateway and OpenAI reference |
| `record_code_interpreter_cassettes.sh` | One code-interpreter calculation; OpenAI blocking/SSE and gateway blocking/SSE/WebSocket | gateway and OpenAI reference |
| `record_messages_tool_choice.py` | Forced `any` and named Messages searches followed by an automatic answer (JSON + SSE) | gateway's upstream traffic to vLLM |
| `record_image_input_cassettes.sh` | Matching two-turn image-input conversations (streaming + non-streaming) | gateway and OpenAI reference |
| `record_text_file_input_cassettes.sh` | Inline text-file turns, `previous_response_id` continuation, and compaction (streaming + non-streaming) | gateway (compaction calls: direct vLLM) and OpenAI reference |
| `record_dynamo_cassettes.sh` | Stateful two-turn and client-executed function tool call cassettes (streaming + non-streaming) | NVIDIA Dynamo frontend |
| `record_dynamo_messages_cassettes.sh` | Two-round Messages web-search cassettes (streaming + non-streaming), validated before replacement | Existing NVIDIA Dynamo frontend |
| `record_sglang_cassettes.sh` | Same shared executor scenarios as Dynamo, staged validation and sanitized provenance | SGLang |
| `record_tool_search_cassettes.sh` | Four-turn mixed function/namespace client tool-search characterization; gateway blocking, HTTP/SSE, and WebSocket acceptance | gateway and OpenAI reference |
| `record_hosted_tool_search_cassettes.sh` | Hosted (`execution: "server"`) tool search over deferred functions, namespaces, and MCP servers: validation, output items and streaming events, `query` filtering, `max_tool_calls`, continuation, `store: false` replay (OpenAI reference only), and WebSocket | gateway and OpenAI reference |

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

### HTTP store:false (OpenAI and gateway)

Run from the repository root with `OPENAI_API_KEY` exported:

```bash
MODEL=gpt-4o uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_store_false_cassettes.sh

cargo test -p agentic-server --test responses_test recorded_store_false_
```

The shell wrapper uses `record_cassette.py` for every captured request and response.
It derives follow-up input from captured outputs and reuses the existing pipeline
function declarations and output fixtures. Each scenario is recorded to its own
standard `turns:` YAML in `store_false/`: `manual-replay`, `stored-parent`,
`unstored-parent`, `conversation`, `function-replay`, and `conversation-items`,
each in JSON and SSE mode. Filenames follow
`store-false-SCENARIO-PROVIDER-MODEL-MODE.yaml`. Each scenario runs against an
isolated gateway in the replay tests. Temporary follow-up input is created under
`TMPDIR` (normally `/tmp`) and removed when the recorder exits.
The default model is `gpt-4o`; the replay tests use those filenames.

To record a running gateway backed by vLLM, select `STORE_FALSE_RECORD_SET=gateway`
and set `GATEWAY_MODEL` to the served model name. It takes precedence over `MODEL`,
which remains a fallback:

```bash
STORE_FALSE_RECORD_SET=gateway GATEWAY_URL=http://127.0.0.1:9000 \
GATEWAY_MODEL=Qwen/Qwen3.8-27B-FP8 \
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_store_false_cassettes.sh
```

Gateway captures use `store-false-SCENARIO-gateway-MODEL-MODE.yaml` filenames,
leaving OpenAI references intact. This uses the existing recorder's unauthenticated
`--gateway` mode; run a local gateway with authentication disabled and database
storage enabled for the stored-parent and conversation cases. No OpenAI key is
needed. `MAX_OUTPUT_TOKENS` defaults to 2048 for gateway recordings and 256 for
OpenAI, and can be overridden for reasoning models. The wrapper does not start
vLLM or the gateway.

Both JSON and SSE cover full-history replay without storage, a stored parent with
an unstored child, rejection of an unstored parent, explicit conversation history,
and a client-executed function with complete history replay. Missing-parent captures
retain OpenAI's HTTP 400 `previous_response_not_found` error. The conversation case
sends `conversation` on both turns, without `previous_response_id`.

Gateway replay checks output items, error envelopes, normalized upstream history,
and database rows. Ordinary `store:false` turns leave no stored response or items;
explicit conversations read stored history without appending the unstored turn, and
stored-parent references hydrate the parent without retaining the new response.
The `conversation-items` scenario adds GET item-listing and response-retrieval
probes around `store:false` turns in both empty and populated conversations. These
recordings establish persistence independently of model-generated answers.
Missing-response GETs check the recorded HTTP 404 error envelope as well as the
absence of stored rows. Both providers' captures replay through the gateway;
model text and reasoning differ, while state controls and function arguments
are compared across providers.

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

### Code interpreter (OpenAI reference and gateway)

The recorder captures one deterministic calculation in exactly five profiles: OpenAI blocking and HTTP/SSE as the
wire-contract reference, plus gateway blocking, HTTP/SSE, and Responses WebSocket. Both providers receive the
`{"type":"code_interpreter","container":{"type":"auto"}}` declaration. The gateway executes generated Python in its
local Eryx sandbox. The captured requests and responses verify the public declaration and lifecycle across all five
profiles.

The characterization test requires the OpenAI event names and lifecycle order, folds provider-dependent code-delta
chunking, checks stable item IDs and output indexes, and rejects events after `response.output_item.done`. It also
checks blocking/streaming transport parity and that the WebSocket messages exactly match the recorder's synthesized
SSE. OpenAI currently returns `outputs: null` for the completed call; the gateway intentionally returns its local
`logs` output containing `CODE_INTERPRETER_OK=385`.

Start an embedded-code-interpreter gateway with a fresh database and the Eryx runtime artifact inside a delegated
Linux cgroup v2 scope, then record the full matrix. This local recipe requires a running systemd user manager. Every
selected recording is staged, checked for unmasked authorization data, and semantically validated before it replaces
a checked-in cassette.

```bash
install -d -m 700 /tmp/agentic-api-code-interpreter

systemd-run --user --scope --quiet --property=Delegate=yes \
  bash scripts/tests/with-code-interpreter-cgroup.sh \
  env ERYX_RUNTIME_CWASM=/path/to/runtime.cwasm \
    TMPDIR=/tmp/agentic-api-code-interpreter \
    AGENTIC_CODE_INTERPRETER_ENABLED=true \
    DATABASE_URL=sqlite:///tmp/agentic_api_code_interpreter_matrix.db \
    cargo run -p agentic-server --features embedded-code-interpreter -- \
      --gateway-host 127.0.0.1 \
      --gateway-port 3098 \
      --llm-api-base http://127.0.0.1:8000 \
      --skip-llm-ready-check
```

```bash
OPENAI_API_KEY=sk-... \
CODE_INTERPRETER_RECORD_SET=all \
OPENAI_MODEL=gpt-5.6 \
GATEWAY_URL=http://127.0.0.1:3098 \
GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B \
bash crates/agentic-server-core/tests/cassettes/record_code_interpreter_cassettes.sh
```

Use `CODE_INTERPRETER_RECORD_SET=openai-reference`, `gateway-nonstreaming`, `gateway-streaming`,
`gateway-websocket`, or `gateway` to refresh only that part of an existing five-profile matrix.

### Web search (gateway and OpenAI)

The default records both providers. Use `WEB_SEARCH_RECORD_SET=gateway` or
`WEB_SEARCH_RECORD_SET=openai` to record only one side.

```bash
OPENAI_API_KEY=sk-... \
bash crates/agentic-server-core/tests/cassettes/record_web_search_cassettes.sh
```

### `max_tool_calls` (OpenAI reference and gateway)

The recorder captures four groups per provider for [#398](https://github.com/vllm-project/agentic-api/issues/398):
`validation` (JSON only; every accepted and rejected value), `builtin` and `counting` (JSON and SSE; web search,
code interpreter, and MCP exhaustion, client-executed calls, and continuations), and `websocket`. Each leg is an
independent conversation appended with `--append`; the script header lists the legs and what each isolates. The
gateway recordings show the gateway enforcing the limit against a local You.com-compatible search stub
(`YOU_API_BASE_URL`), so their search results are synthetic.

```bash
OPENAI_API_KEY=sk-... \
bash crates/agentic-server-core/tests/cassettes/record_max_tool_calls_cassettes.sh

MAX_TOOL_CALLS_RECORD_SET=gateway GATEWAY_URL=http://localhost:9000 GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B \
MAX_OUTPUT_TOKENS=16384 MAX_TOOL_CALLS_SKIP_LEGS="mixed-builtin mcp-failure mcp-then-search" \
bash crates/agentic-server-core/tests/cassettes/record_max_tool_calls_cassettes.sh
```

`MAX_TOOL_CALLS_RECORD_SET` selects `openai` (default), `gateway`, or `all`; `MAX_TOOL_CALLS_GROUPS` selects
groups; `MAX_TOOL_CALLS_SKIP_LEGS` omits legs a provider cannot run. The gateway `code-interpreter` legs need the
embedded-code-interpreter gateway described above.

### Hosted tool search (OpenAI reference and gateway)

The recorder captures hosted tool search (`{"type": "tool_search"}`), run by OpenAI and by the gateway, for
[#448](https://github.com/vllm-project/agentic-api/issues/448). `hosted_tool_search/` holds one recording per group
(the gateway has no `replay` group), provider, and mode, named `hosted-tool-search-<group>-<provider>[-<mode>].yaml`,
where the provider is `openai-reference-gpt-5.6` or `gateway-Qwen-Qwen3.6-35B-A3B-FP8`. The recorder's inputs live in
`hosted_tool_search/inputs/`: the tool catalogs and the fake function outputs. The MCP failure legs share
`tools-mcp-unreachable.json`; the recorder derives each variant (URL, eager, or an `allowed_tools` filter that matches
nothing) at record time. Each recording keeps its full request bodies.

| Group | Modes | Legs | What it proves | Checked by (`hosted_tool_search_characterization_test.rs`) |
|---|---|---|---|---|
| `validation` | JSON | 6 | Which declarations and `tool_choice` values are refused | `validation_errors_match_openai`, `gateway_validation_matches_openai_except_the_intended_mcp_choice` |
| `contract` | JSON, SSE | 6 | Item shapes, an MCP server searched as `mcp_<label>`, streaming events, `max_tool_calls`, continuation | `deferred_mcp_server_loads_as_a_namespace`, `streaming_has_no_dedicated_tool_search_events`, `search_does_not_count_toward_max_tool_calls`, `gateway_search_items_have_the_reference_shape_and_pairing` |
| `query` | JSON | 21 | How the `query` argument filters the loaded tools | `query_is_a_case_insensitive_substring_filter_within_paths`, `gateway_loads_the_same_tools_as_openai_for_the_same_arguments` |
| `edges` | JSON | 6 | Repeat searches in one response and across responses; an unreachable MCP server | `edge_cases_match_openai`, `gateway_restores_loads_on_continuation` |
| `mcp-failures` | JSON, SSE | 7 | A failing MCP listing fails the request (424); a filter that matches nothing completes | `openai_fails_the_request_when_any_mcp_listing_fails`, `gateway_handles_mcp_listing_failures_exactly_like_openai` |
| `replay` | JSON | 1 | A `store: false` transcript replayed as input loads without searching again (OpenAI reference only: the transcript holds `mcp_call` items, which the gateway does not convert for vLLM yet) | `loaded_tools_persist_without_a_new_search` |
| `websocket` | WebSocket | 1 | The streaming events over a WebSocket | `streaming_has_no_dedicated_tool_search_events`, `gateway_streams_searches_with_only_added_and_done_events` |

Each leg is an independent conversation appended with `--append`; the script header lists the legs and what each
isolates. Hosted `tool_search_call` items have `call_id: null` and are not client work, so the recorder does not
answer them.

```bash
OPENAI_API_KEY=sk-... \
bash crates/agentic-server-core/tests/cassettes/record_hosted_tool_search_cassettes.sh
```

The gateway recordings use Qwen/Qwen3.6-35B-A3B-FP8 on vLLM. Start the gateway with a fresh database and let it reach
the recorded MCP hosts (`gitmcp.io`, the unresolvable `mcp.invalid`, and the failing `httpbin.org`, `example.com`,
and `api.githubcopilot.com`), then record:

```bash
AGENTIC_API_HOME=/tmp/agentic-hosted-search DATABASE_URL=sqlite:///tmp/agentic-hosted-search/hosted.db \
AGENTIC_MCP_ALLOWED_HOSTS=gitmcp.io,mcp.invalid,httpbin.org,example.com,api.githubcopilot.com \
cargo run -p agentic-server -- --gateway-port 3097 --llm-api-base http://127.0.0.1:8000/v1 --skip-llm-ready-check

HOSTED_TOOL_SEARCH_RECORD_SET=gateway GATEWAY_URL=http://127.0.0.1:3097 \
GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 MAX_OUTPUT_TOKENS=16384 \
bash crates/agentic-server-core/tests/cassettes/record_hosted_tool_search_cassettes.sh
```

`HOSTED_TOOL_SEARCH_RECORD_SET` selects `openai` (default), `gateway`, or `all`; `HOSTED_TOOL_SEARCH_GROUPS` selects
groups; `HOSTED_TOOL_SEARCH_SKIP_LEGS` omits legs. Provider errors (including the 400s the validation group expects)
are kept as evidence. The two models choose different searches, so
`hosted_tool_search_characterization_test.rs` compares the gateway's item shapes, streaming lifecycle, and the tools
it loads for arguments identical to the reference's.

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

### Text file input (gateway → vLLM text model, and OpenAI)

Every turn carries `text_files/inputs/notes.txt` as an inline `input_file` (`data:text/plain;base64,...`). Its release
codeword `PAPAYA-7` appears only in the file, and every recording is validated to answer with it, so a cassette shows
that the model read the attachment. `text_file_input_test.rs` replays each recording through the gateway against a
mock upstream and checks that the model receives a `File: notes.txt` `input_text` part with no `input_file` or
`file_data`, that stored history and the compacted window keep the client's original `input_file`, and that a
`previous_response_id` continuation decodes the stored file again. Every scenario is recorded for the gateway and,
as the reference, against the OpenAI Responses API (`gpt-4o`).

| Scenario | Recorded through | What it proves |
|---|---|---|
| `single` | gateway, streaming + non-streaming | the question and inline file (`inputs/single-file.json`) reach the model as text; storage keeps the file |
| `continuation` | gateway, streaming + non-streaming | a text follow-up by `previous_response_id` re-inlines the stored file |
| `compact` + `compact-followup` | direct vLLM, non-streaming | the summarizer receives exactly the decoded-text input (`inputs/compact-text-file.json`), and a follow-up on the compacted window answers from the summary |

The gateway makes its compaction calls internally, so those two recordings capture the model-facing requests directly
against the model server, like `compaction/`. The follow-up input is built from the recorded summary. The replay checks
that the gateway, given the original `input_file`, sends exactly the recorded summary input, and the recorded
follow-up input apart from the `id` and `status` that compaction assigns to the retained message.

**Recorded configuration.** vLLM 0.32.0 serving `Qwen/Qwen2.5-3B-Instruct` on one 12 GB GPU (RTX 4070 Ti, WSL2),
started with `vllm serve Qwen/Qwen2.5-3B-Instruct --port 8000 --max-model-len 8192 --gpu-memory-utilization 0.85
--enable-auto-tool-choice --tool-call-parser hermes` (no tools are declared in these scenarios),
and the gateway started with `cargo run -p agentic-server -- --llm-api-base http://127.0.0.1:8000` on port 9000.

```bash
# Gateway set (no API key): conversations through the gateway, compaction calls against vLLM.
GATEWAY_URL=http://localhost:9000 VLLM_URL=http://localhost:8000 MODEL=Qwen/Qwen2.5-3B-Instruct \
TEXT_FILE_RECORD_SET=gateway \
bash crates/agentic-server-core/tests/cassettes/record_text_file_input_cassettes.sh

# OpenAI reference set.
OPENAI_API_KEY=sk-... TEXT_FILE_RECORD_SET=openai \
bash crates/agentic-server-core/tests/cassettes/record_text_file_input_cassettes.sh
```

Use `TEXT_FILE_SCENARIOS="compact"` (space-separated) to record a subset, and `OPENAI_MODEL` to change the reference
model. Recordings are staged and replace final files only after every selected recording validates. The replay test
requires both sets: it replays the gateway and OpenAI recordings and checks that the gateway builds the same
model-facing file turn and summary input for both. A different model changes the file names; update
`GATEWAY_MODEL_SLUG` or `OPENAI_MODEL_SLUG` in `text_file_input_test.rs` to match.

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

### Async client tools (OpenAI reference and gateway)

See [the async tool recording guide](async_tools/README.md) for what each recording shows, how to record, and the
findings. `record_async_tool_cassettes.py` records an OpenAI reference set (`gpt-6-astra`) that pins the public
contract, and a gateway set that replays the same scenarios through the gateway for a step-by-step comparison. Each
scenario records only the modes that answer a distinct question. With `--provider vllm --sample N` it measures, without
recording, how a model server continues around a pending call with and without the gateway's hints.

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
