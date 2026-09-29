# Multi-agent reference recordings

Run `../record_multi_agent_cassettes.sh` to record OpenAI before implementing the
gateway contract. `MULTI_AGENT_RECORD_SET=openai` is the default. Set
`MULTI_AGENT_SUITE=all` (default) or `workflows` to select all task scenarios.
Select `review`, `proposals`, or `mixed-tools` to record just one scenario in JSON and SSE.

The suite writes six YAML files per provider/model: one JSON and one SSE
cassette for each of `review`, `proposals`, and `mixed-tools`.
Filenames include the provider, scenario, model, and transport mode.

The proposal and mixed-tools workflows continue pending client tool calls using
requests linked by `previous_response_id`, up to `MULTI_AGENT_MAX_CONTINUATIONS`
(default: 10). Review uses one request. Each scenario starts with separate delegated tasks.

The existing `multi-agent-openai-reference-review-gpt-5.6-sol-nonstreaming.yaml`
is the accepted behavioral baseline. The old `edge-cases`, `failure-cases`, and
`compaction` files used a prompt that explicitly prohibited delegation. They must
not be used as evidence for multi-agent runtime behavior or gateway acceptance tests.
The script no longer generates those groups or supports the `validation` suite.
Existing recordings are left untouched; further evidence must come from new API recordings.

## Scenario prompts

All prompts live in `prompts.txt`, under `[review]`, `[proposals]`, `[mixed-tools]`,
and `[client-owned-tools]`. Each nonempty line within a section is one user prompt.
Each section has one prompt. The shell script
checks the selected section's prompt count before starting the recorder.

## Capture boundary

Each workflow starts with one client prompt describing multiple jobs. The review
prompt asks for correctness analysis, security analysis, and test recommendations.
The proposal prompt asks for independent alpha and beta assessments, followed by
a combined recommendation. OpenAI decides how to assign and coordinate the work;
the driver does not issue a separate request or task prompt for each subagent.

The recording proxy captures the client-facing exchange under `turns`, retaining
all response items and the global SSE order, including unknown item/event kinds
and server errors. It does not create agent indexes or reorganize data by agent.
Agent views and behavior assertions belong in separate analysis/replay tooling.
Credentials remain masked in captured request headers.

The scenario driver constructs requests and supplies function outputs and simulated
local-shell outputs from fixed fixtures. OpenAI owns agent creation, task assignment,
collaboration, and scheduling. The proxy does not implement those decisions or
change a response to match the scenario's expectations.

The continuation limit is a recording budget, not proof that the agent tree has
finished. Reaching it with pending client calls fails the driver and retains the
captured exchanges for inspection.

## Gateway beta scope

The gateway supports multi-agent continuations for client-executed functions,
local shell, custom tools, and tool search. Search results load definitions only
for the agent that owns the call; discovered namespace members keep their public
namespace on output. Outputs are matched by call ID and kind before agents resume.

An explicit `compaction_trigger` compacts only the root's resolved context,
returns a root-attributed compaction item, and preserves children and pending calls.
It does not start agent inference. Automatic per-agent compaction remains enabled. OpenAI documents
`/responses/compact` as unsupported with multi-agent; that endpoint restriction
does not establish a restriction on the `compaction_trigger` input marker.
The client-owned-tools scenario contains tool search, functions, and custom tools.
The mixed-tools scenario contains only web search, MCP, and local shell.

## Isolated client-owned-tools diagnostic

Select `MULTI_AGENT_SUITE=client-owned-tools` to record the `[client-owned-tools]`
section of `prompts.txt`, in a fresh stored Responses session for each mode. This diagnostic is opt-in and is not included in `all/workflows`.

It uses `client_owned_tools.json`: the existing tool-search catalog plus the custom
`agentic_raw_echo` tool. Three agents are requested for weather, time zone, and a
custom-tool output reported on one line. Returned definitions come from
`../tool_search/returned_tools.json`; callbacks in `mixed_tool_outputs.py` reuse the
existing function and custom-tool output fixtures. Web, MCP, and shell are not declared.
Discovery outputs and subsequent function outputs are submitted through ordinary
HTTP continuations with matching call IDs.

```bash
MULTI_AGENT_RECORD_SET=openai \
MULTI_AGENT_SUITE=client-owned-tools \
MULTI_AGENT_STREAM_MODE=both \
MAX_CONCURRENT_SUBAGENTS=3 \
HTTP_READ_TIMEOUT=900 \
bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

This produces separate `multi-agent-openai-reference-client-owned-tools-<model>-<mode>.yaml`
files and leaves mixed-tools captures unchanged. Inspect function-call ownership
and final answers: a completed HTTP response alone does not establish task success.

## Mixed tools: web search, MCP, and shell

`MULTI_AGENT_SUITE=mixed-tools` supplies one prompt requesting three agents for
Python CSV web research, GitMCP tiktoken research, and the local-shell command
`python3 -c 'print(sum(range(1, 11)))'`. `mixed_tools.json` declares only web search,
MCP, and local shell. Tool search and custom tools belong to client-owned-tools.

The request sets `max_concurrent_subagents: 3`. The shell callback in
`mixed_tool_outputs.py` supplies simulated stdout `55\n`, empty stderr, and exit
code 0 for the exact command. Unsupported commands receive explicit simulated
failures; nothing is executed. Automatic continuations submit matching shell
outputs with `previous_response_id` and add no user messages. Responses and SSE
events are captured without modification.

The MCP declaration uses `https://gitmcp.io/openai/tiktoken`, server label
`gitmcp_tiktoken`, and allowed tool `search_tiktoken_documentation` with approval
set to `never`. No GitHub token is configured by this scenario.

With your Python environment active and `OPENAI_API_KEY` exported:

```bash
MULTI_AGENT_RECORD_SET=openai \
MULTI_AGENT_SUITE=mixed-tools \
MULTI_AGENT_STREAM_MODE=both \
MAX_CONCURRENT_SUBAGENTS=3 \
HTTP_READ_TIMEOUT=900 \
bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

This writes the usual two mixed-tools YAML files per provider/model, containing
the user prompt and all client-tool continuations. Re-recording replaces the
selected files. A longer read timeout allows slow nonstreaming responses; it does
not establish that the tool combination works. Inspect actual calls, errors, and
completion after recording. Existing captures are not changed by editing fixtures.

## Runtime coverage still to record

These require dedicated scenarios with actual delegated work before they can be
claimed as behavioral coverage:

- Edge cases: concurrent task limits and continuation with real pending calls,
  including partial, duplicate, or mismatched outputs.
- Failures: a delegated task encountering a tool execution failure and the
  resulting behavior of sibling agents and the root.
- Compaction: enough conversation context to trigger compaction with multi-agent
  enabled, followed by continuation that exercises retained task state.

The compaction recording must show that compaction actually occurred; merely
setting a threshold does not establish coverage. The capture remains passive;
coverage is assessed from recorded results afterward. Gateway multi-agent support
remains scoped to `store: true`.

Re-recording replaces the selected scenarios. `--dry-run` previews commands without
writing files or contacting an API. Input fixtures in this directory are required
by the recorder; keep them when removing recorded YAML files.

To retry only mixed-tools over SSE:

```bash
MULTI_AGENT_RECORD_SET=openai \
MULTI_AGENT_SUITE=mixed-tools \
MULTI_AGENT_STREAM_MODE=streaming \
HTTP_READ_TIMEOUT=900 \
MAX_CONCURRENT_SUBAGENTS=3 \
bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

Use `MULTI_AGENT_STREAM_MODE=nonstreaming` for the JSON recording, or `both`
(the default) for both modes. The multi-agent script read timeout defaults to 900 seconds and measures
time waiting for network data, not the total duration of the agent tasks. Changing
it affects recorder transport settings only; it does not change the API payload.
The local client allows an extra ten seconds for the proxy to report an upstream
read timeout. If a recording fails, the script still attempts the other selected modes
and scenarios, then lists failures and exits nonzero. Transport failures are recorded as `response.transport_error`, without
inventing an upstream HTTP status or response body. Such a capture is diagnostic
evidence, not a successful multi-agent response cassette.

For non-streaming requests, the recorder prints elapsed-time updates while
waiting for the HTTP response. Reading a prompt from a redirected file does not
require terminal input. The proposals and mixed-tools scenarios submit pending tool outputs automatically
after the initial prompt, without requiring additional scripted prompt lines.

## Recorded integration comparison

These integration tests read captures without making live API calls.

Run the comparison suite:

```bash
cargo test --locked -p agentic-server-core --test multi_agent_contract_test
```

Five Nemotron captures pass the session contract comparison: review in both
transports, mixed-tools in both transports, and streaming proposals. Each session
must preserve call/result identity, attribution, continuation chaining, and SSE
item lifecycle. The gateway must exercise the reference's tool kinds and finish
with a root final answer and no pending client calls. Model-generated wording,
identifiers, action counts, and the number of client-tool continuation requests
may differ. These checks do not grade answer quality or prove inference concurrency.

The nonstreaming proposals capture is a known failed scenario: the model emits
`<to=spawn_agent>` as message text and never retrieves either proposal. Its test
asserts rejection, not successful parity. Keep it as evidence until a new live
recording replaces it; do not edit the captured response into a passing example.

## Large streaming capture

The Nemotron review stream has 52,112 events and 53,281 reported output tokens.
Its original 16,513,908-byte YAML is stored as a 730,223-byte `.yaml.gz` file.
Compression is lossless; no deltas, reasoning, or terminal snapshots were removed.
The integration loader accepts both `.yaml` and `.yaml.gz`.

SHA-256 of the uncompressed review YAML:

```text
37a70274d2458f135452e6d9e2701848e390267442f1f03bd2aed0236bb99632
```

To inspect it without changing the tracked capture:

```bash
gzip -dc multi-agent-gateway-review-nvidia-NVIDIA-Nemotron-3-Nano-30B-A3B-BF16-streaming.yaml.gz | less
```

The recorder still writes ordinary YAML and logs. After re-recording review
streaming, replace the compressed artifact with a losslessly compressed copy,
update the size/hash above, and rerun the integration suite. Do not retain both
the old compressed capture and a new uncompressed capture as competing fixtures.
