# Multi-agent reference recordings

Run `../record_multi_agent_cassettes.sh` to record OpenAI before implementing the
gateway contract. `MULTI_AGENT_RECORD_SET=openai` is the default. Set
`MULTI_AGENT_SUITE=all` (default) to select all task scenarios, plus both edge-case suites
when using WebSocket transport. Select `review`, `proposals`, `mixed-tools`,
`client-owned-tools`, or `code-interpreter` to record just one task scenario.

The HTTP suite writes ten YAML files per provider/model: one JSON and one SSE
cassette for each task scenario. WebSocket `all` writes seven files per provider/model.
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

## Persistent WebSocket recordings

Set `MULTI_AGENT_TRANSPORT=websocket` to use the same five prompt sections and
tool fixtures as the HTTP suite. `record_multi_agent_cassettes.sh` still invokes
`record_cassette.py`; its multi-agent WebSocket path uses
`websocket_recorder.py`. No separate recording entry point is needed.

From the repository root, with `OPENAI_API_KEY` already exported:

```bash
MULTI_AGENT_TRANSPORT=websocket \
MULTI_AGENT_RECORD_SET=openai \
MULTI_AGENT_SUITE=all \
OPENAI_MODEL=gpt-5.6-sol \
uv run --no-project \
  --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

After inspecting those files, record the gateway using the same prompts:

```bash
MULTI_AGENT_TRANSPORT=websocket \
MULTI_AGENT_RECORD_SET=gateway \
MULTI_AGENT_SUITE=all \
GATEWAY_URL=http://localhost:9000 \
GATEWAY_MODEL=your-served-model \
uv run --no-project \
  --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

With `MULTI_AGENT_SUITE=all`, each invocation writes five
`multi-agent-<provider>-<scenario>-<model>-websocket.yaml` task files plus the
`ws-edge-cases` and `ws-active-text-edge-cases` files, for seven YAMLs per provider.
`MULTI_AGENT_EDGE_TIMEOUT` applies to both edge-case suites. The HTTP files are untouched.
`MULTI_AGENT_STREAM_MODE` applies only to HTTP; WebSocket has one duplex mode.
Use `MULTI_AGENT_SUITE=proposals` to isolate live function-output injection, or
append `--dry-run` to preview requests without connecting or writing files.
Re-running a scenario replaces its WebSocket file. `MULTI_AGENT_OUTPUT_DIR`
can preserve separate recording attempts.

These captures use `format: responses-websocket-v1` and a `sessions` list,
not HTTP `turns`. One connection remains open for the entire scenario, including
any explicitly rejected late-output continuation. The handshake records the actual
upgrade status and response headers; authorization is masked and Set-Cookie is
excluded. Each observed frame retains its direction, ordinal, elapsed time, opcode,
FIN flag, and exact UTF-8 text or base64 payload. Ping, pong, fragmentation, and close
frames remain in order. Nothing is converted to synthesized SSE or `[DONE]`.

The driver submits fixture outputs on `response.output_item.done`, continues
reading while an injection is outstanding, and waits for both the terminal response
and its injection outcomes. It allows one outstanding injection per response so
acknowledgements without batch IDs can be associated unambiguously. Only input
explicitly returned with `response_already_completed` is submitted in a create
chained by `previous_response_id`. Transport loss never retries an uncertain batch.

The proposals fixture supplies functions. Mixed-tools and client-owned-tools also
submit shell, custom-tool, and discovery outputs as characterization probes; support
for those injection kinds is characterized by the recordings. The initial gateway
captures rejected shell and discovery outputs at schema decoding. The latest gateway
captures confirm acceptance of shell, discovery, function, and custom outputs after
typed decoding and discovery refresh were fixed. Code interpreter requires the same
enabled gateway runtime as its HTTP scenario. The recorder never executes the simulated shell commands.

Frames are flushed to disk as they arrive. Failure, interruption, or timeout retains
the observations and records a close/failure outcome when cleanup runs; a killed
process can leave that outcome absent. The capture budgets are 250,000 frames,
128 MiB of frame payloads, 16 MiB per frame/message, 64 queued client outputs, and
100 injections per create. `HTTP_READ_TIMEOUT` also controls WebSocket socket
inactivity (default 900 seconds in the suite); close-handshake observation is bounded
to five seconds. These limits are recorder policy, not claims about OpenAI.

Live captures and their comparison are available below. Full gateway replay of this
duplex format and race characterization remain necessary before claiming conformance.
Existing HTTP replay tests do not consume this new format.

Run the hermetic recorder tests without an API key:

```bash
uv run --no-project \
  --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  python -m unittest discover -s crates/agentic-server-core/tests/cassettes -p 'test_record*.py'
```


## WebSocket comparison with the latest gateway recordings

The five OpenAI and five Qwen gateway WebSocket files use matching initial prompts.
Per-response sequence numbers increase in all ten files. All ten close cleanly with
no server errors or outstanding injection acknowledgements. All five gateway scenarios
complete with a root final answer. The comparison intentionally
distinguishes a completed response from a successfully finished delegated task.

| Scenario | OpenAI capture | Gateway capture |
| --- | --- | --- |
| Review | Three delegated agents, one root final answer | Three delegated agents, one root final answer |
| Proposals | Two delegated agents, two accepted function injections, root final answer | Same observed transport milestones |
| Mixed tools | Accepted shell injection; web search and MCP work; root final answer | Accepted shell injection; web search and MCP work; root final answer |
| Client-owned tools | Three accepted injections (discovery then two functions); two late rejections; two completed responses, **no root final answer** | Two late discovery rejections with unchanged input carried into a continuation; three accepted injections (two function, one custom); two completed responses and one root final answer |
| Code interpreter | Two delegated agents, web research and two interpreter calls, root final answer describing four distinct problems | Two delegated agents, five web searches, five completed interpreter calls with logs, root final answer covering **four distinct** problems |

The client-owned OpenAI capture returns custom-tool outputs intact in a late
`response_already_completed` batch; it does not prove live custom-output acceptance.
Returned late input is carried into the second create. Neither a clean close nor the
recorder's completed status establishes that the root produced its requested summary.
The latest gateway capture also takes the late discovery path: its root requests two
discovery calls before delegating, and the response completes before either injection
is accepted. Both rejected outputs are returned unchanged and submitted together with
`previous_response_id` on the same socket. That continuation discovers the functions,
delegates all three tasks, accepts their function/custom outputs, and produces the
requested weather, time zone, and custom marker in its final answer. This differs from
the previous capture's live discovery path; deterministic tests still cover live discovery.
Both existing gateway HTTP code-interpreter captures produce four distinct problems.
The latest WebSocket capture also satisfies this requirement: one child solves Two Sum
and Merge Two Sorted Lists; the other solves Valid Parentheses and Longest Palindromic
Substring. All four have recorded execution outputs and appear in the root's final
summary. The fifth interpreter call produces the second child's consolidated report.

The previous WebSocket attempt duplicated both problems across children. Shared model
guidance was strengthened to request explicit disjoint assignments and verification of
combined count, distinctness, coverage, and execution evidence. This new capture shows
a successful run after that change; a single recording does not guarantee model reliability.
Token usage and generated wording also differ; these recordings do
not isolate transport performance.

The gateway schema failures motivated typed shell/custom/discovery injection support.
A deterministic gateway test additionally exposed a tool-discovery refresh defect:
accepting the output alone did not make the newly discovered function available in
the next inference round. That path now reuses core tool-search preparation and registry
construction, without resetting the agent's round budget. Deterministic live injection
tests cover discovery followed by function execution, shell, custom, and returned late input.

Read-only capture audits are reproducible with:

```bash
cargo test -p agentic-server-core --test multi_agent_websocket_cassette_test
```

The audit tests assert matching prompts, delegation, response ordering, clean close,
acknowledgement completion, and the successful gateway injection kinds. They preserve
the incomplete OpenAI client-owned task as an explicit observation. They do not grade
model answers or establish full parity. Full duplex dependency replay and
compaction/race characterization remain separate work.

## WebSocket edge-case characterization

WebSocket `all` includes this suite and its active variant. To record only edge cases,
select `MULTI_AGENT_SUITE=websocket-edge-cases`; it records four isolated sessions using the
same raw `RecordedSession` recorder. The edge driver lives in `websocket_recorder.py`,
invoked by the existing shell script. All four cases write to one YAML per provider:
`multi-agent-<provider>-ws-edge-cases-<model>-websocket.yaml`. Its `sessions` list
contains one connection per case, labeled by `handshake.probe.case`. Frames are flushed
as they arrive, preserving earlier sessions if a later probe fails or is interrupted.
Re-running the full suite replaces that combined file. No existing positive capture is replaced. Export `OPENAI_API_KEY`, then run:

```bash
MULTI_AGENT_TRANSPORT=websocket \
MULTI_AGENT_SUITE=websocket-edge-cases \
MULTI_AGENT_RECORD_SET=openai \
OPENAI_MODEL=gpt-5.6-sol \
uv run --no-project \
  --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

For gateway, use `MULTI_AGENT_RECORD_SET=gateway`, `GATEWAY_URL`, and `GATEWAY_MODEL`
as in the positive suite. `GATEWAY_API_KEY` is optional for authenticated gateways.
`MULTI_AGENT_EDGE_CASE` selects one case below (default `all`); a single-case run writes
`multi-agent-<provider>-ws-edge-<case>-<model>-websocket.yaml` to preserve the combined suite.
`MULTI_AGENT_EDGE_TIMEOUT` defaults to 120 seconds per probe. `--dry-run` lists
outputs without opening connections. The suite focuses on completion races, duplicate
acceptance, and batch atomicity; basic schema and parameter validation probes are omitted.

| Case | Observation sought |
| --- | --- |
| `late-continuation` | Withhold a real call output until terminal, inject it, then continue with exactly the input explicitly returned by `response_already_completed` |
| `duplicate-in-batch` | Two copies of the same output inside one batch |
| `mixed-valid-invalid` | A real output plus an unknown call; after explicit rejection, submit the real output alone to probe batch atomicity |
| `duplicate-injection` | Deliberately send the same output twice back-to-back and observe both outcomes, including acknowledgements after terminal |

The [OpenAI multi-agent guide](https://developers.openai.com/api/docs/guides/responses-multi-agent#websocket)
documents unknown/completed response errors and schema errors with connection closure.
These probes record exact payloads, returned input, ordering, and close frames without
assuming undocumented error codes. Generic errors start a five-second server-close
observation window. If no close arrives, the capture reports a driver/transport failure;
that is an inconclusive close observation, not a fabricated provider rejection. EOF,
timeout, and socket failures are retained and never trigger an uncertain retry. One
failed probe does not prevent recording the others; cancellation still stops the suite.
A captured server close ends that probe, but does not mean the intended condition was
reached: inspect preceding frames for authentication or unrelated errors.

The live-call probes use one root-owned `edge_echo` function to isolate admission
semantics from delegation quality. If the model does not call it, the prerequisite
failure is reported. If completion wins the race, `response_already_completed` only
characterizes late input; it does not establish active-call validation. A timed-out
late-continuation probe may mean the provider keeps the response open awaiting input.
Completed recording means observations were captured, not that the provider passed
an expected behavior assertion. Cross-connection ownership, forced compaction races,
queue saturation, and disconnect during acceptance still need separate characterization.

### Latest edge-recording comparison

All four cases have matching prompts and tool declarations. Both captures have strictly
increasing per-response sequence numbers, an acknowledgement for every injection,
unchanged input in rejections, and clean closes. Neither contains a generic server error.

| Case | OpenAI | Gateway |
| --- | --- | --- |
| Late continuation | Completes with a pending root function call; late injection returns `response_already_completed` and unchanged input. A second create on the same socket resumes and returns `EDGE_OK`. | Same observed behavior; the previous 120-second timeout is resolved. |
| Duplicate within one batch | Completion wins; returns the whole batch with `response_already_completed`. | Same observed behavior. |
| Mixed valid/invalid batch | Completion wins; returns the whole batch with `response_already_completed`. | Same observed behavior. |
| Two identical injections | First accepted; second returns `response.inject.failed` / `invalid_input` and unchanged input. The response finishes with `EDGE_OK`. | Completion wins before either injection is accepted; both receive `response_already_completed` with unchanged input. No generic 400 or server-initiated close. |

These quiescent captures confirm completion and late continuation. The duplicate
case took different race paths, so they do not establish live duplicate-rejection parity.

The separate OpenAI active recording now characterizes all three live cases:

- Duplicate within a batch returns `invalid_input` with `Tool call '<id>' already has an output.`
- Mixed valid/unknown calls return `invalid_input` with `Tool call '<id>' is not pending on response '<response_id>'.`
- Repeated injection accepts the first output and rejects the second with `invalid_input`.

Both invalid batches return their entire input unchanged. A subsequent injection of the
valid member succeeds on the same socket before completion, establishing atomic rejection.
Gateway maps these core decisions to the recorded errors through the existing relay.
The deterministic `websocket_invalid_live_batches_are_atomic_and_do_not_close` test
holds sibling inference active and verifies rejection, retry acceptance, duplicate rejection,
continued completion, and exactly one committed output.

The current paired `ws-active-text-edge-cases` recordings use identical text prompts,
tools, token limits, and multi-agent settings. All three active cases now agree on
acknowledgement ordering, error codes/messages (apart from generated identifiers), and
unchanged returned input. Both invalid batches allow a successful valid-only retry on
the same socket; repeated injection accepts once and rejects once. Late continuation
also matches. No generic server errors occur, and response sequence numbers increase.

Both providers now complete every case and acknowledge all injections. The OpenAI
rerun used a 600-second recorder budget, resolving the earlier recording timeouts.
Response completion counts match (two for late continuation, one for each active
case), and both connections close cleanly after the client requests closure.
Gateway's root answers are `EDGE_OK`; OpenAI uses `EDGE_OK.` in the mixed-batch
case and `EDGE_OK` elsewhere. The audit retains this minor instruction-following
difference separately from successful lifecycle parity.

```bash
cargo test -p agentic-server-core --test multi_agent_websocket_cassette_test
```

### Keep the response active for semantic validation

Set `MULTI_AGENT_EDGE_ACTIVE=true` to use the `[websocket-active]` prompt from
`prompts.txt`. The sibling writes a long guide while the root calls `edge_echo`.
The driver waits for a fresh child-attributed `response.output_text.delta` after
observing the pending call, and refuses injection after sibling finalization or response
completion. This remains a timing-sensitive live probe, not a guaranteed execution barrier.
The late-continuation case keeps its original prompt. No code interpreter is required.

```bash
MULTI_AGENT_TRANSPORT=websocket \
MULTI_AGENT_SUITE=websocket-edge-cases \
MULTI_AGENT_EDGE_ACTIVE=true \
MULTI_AGENT_RECORD_SET=openai \
OPENAI_MODEL=gpt-5.6-sol \
uv run --no-project \
  --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

This writes `multi-agent-openai-reference-ws-active-text-edge-cases-gpt-5.6-sol-websocket.yaml`.
Use the usual gateway provider/model/URL variables for its paired recording. These files
preserve the earlier quiescent and interpreter captures. Reruns skip completed conclusive
cases and append attempts for missing or inconclusive cases to the same YAML. Selecting
`MULTI_AGENT_EDGE_CASE` also uses that file. Historical attempts remain intact, so the file
can contain more than four sessions. A retry starts a new response; it never resubmits an
uncertain batch to its previous response. Archive the YAML to deliberately record every
case again or change models/prompts for a fresh comparison. Malformed/interrupted YAML
is rejected instead of overwritten. `close_outcome.probe` records observed sibling
activity, control outcomes, and whether terminal had already arrived. A missing activity
barrier is reported as an inconclusive probe; all-late acknowledgements remain inconclusive
for active validation even when the sibling was observed streaming. No model assertion
or latency measurement is treated as proof of compaction.

### Deterministic compaction and duplex replay coverage

The live-control unit tests now force injection before a summary snapshot, during an
upstream summary blocked on an explicit barrier, and after compaction commits. They use
`RunControl` admission, check stale-summary rejection, preserve unresolved call/output
linkage, reject a second application, and charge summary usage. Additional barrier tests
verify that a sibling accepts output while the root compacts, mail invalidates the root
snapshot without being lost, and disconnect cancels control without publishing a response.
These are gateway tests, not recordings of OpenAI's internal scheduling.

```bash
cargo test -p agentic-server-core --lib live_control
cargo test -p agentic-server --test responses_websocket_test recorded_edge_sessions_drive_real_gateway
```

The Rust duplex test loads all four gateway edge sessions, drives their actual client
create/inject/close frames through the real gateway, remaps generated response IDs, and
uses ordered server milestones as barriers. It checks output content, call IDs, rejection
input/codes, sequence monotonicity, close behavior, and inference request count. Token-delta
chunk boundaries are not required to match. **Its model fixtures are derived from public
server frames and are explicitly synthetic.** It covers client-transcript replay but does
not establish full replay from independently recorded model and hosted-tool dependencies.

### Capture model dependencies for full replay

The existing HTTP proxy can now run as a separate mode of `websocket_recorder.py`.
In another terminal, set `MODEL_UPSTREAM_URL` to the gateway's actual model base URL,
then run (the output is replaced on startup):

```bash
uv run --no-project \
  --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  python crates/agentic-server-core/tests/cassettes/websocket_recorder.py dependencies \
  --upstream "$MODEL_UPSTREAM_URL" --port 7071 \
  --output /tmp/multi-agent-websocket-model-dependencies.yaml
```

Restart the gateway with `--llm-api-base http://127.0.0.1:7071` and its usual arguments,
then run the gateway WebSocket recorder. The proxy retains actual model requests and
JSON/SSE responses, masks authorization, and assigns unique request IDs on admission
so concurrent completions cannot reuse IDs. Stop it after recording. This produces a
separate model dependency sidecar; it is not a new provider reference or a replacement
for the duplex YAML. Hosted-tool dependencies, request matching, and replay barriers
for concurrent dependency completion still need to be added for **full dependency-backed
replay**. The present captures alone do not supply that evidence.
