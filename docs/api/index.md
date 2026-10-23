# API Reference

## Authentication

Inbound authentication is optional. When the gateway starts with both `OIDC_ISSUER` and `OIDC_AUDIENCE`, every
`/v1/*` HTTP route and the `/v1/responses` WebSocket upgrade require an OIDC `Authorization: Bearer <token>`.
`/health` and `/ready` remain public. Supplying only one OIDC setting is a startup error.

The gateway treats `OIDC_AUDIENCE` as the complete audience trust set: every `aud` value must equal it, and any
present `azp` value must also equal it. It also validates the token signature, issuer, subject, expiration, and
not-before time. The identity token is consumed at the gateway boundary instead of being forwarded to the inference
service. WebSocket sessions reject new `response.create` messages after the validated token expires.

Missing or rejected credentials return `401 Unauthorized` with `WWW-Authenticate: Bearer`. OpenAI-compatible routes
use this envelope:

```json
{
  "error": {
    "message": "invalid bearer token",
    "type": "authentication_error",
    "param": null,
    "code": "invalid_token"
  }
}
```

`/v1/messages` and `/v1/messages/count_tokens` use the Anthropic-compatible envelope:

```json
{
  "type": "error",
  "error": {
    "type": "authentication_error",
    "message": "invalid bearer token"
  },
  "request_id": "req_019..."
}
```

The same `req_`-prefixed identifier is returned in the `request-id` response header.

A JWKS refresh failure returns `503 Service Unavailable`, without `WWW-Authenticate`, so clients can distinguish an
identity-provider dependency failure from rejected credentials. See
[OIDC bearer authentication](../design/oidc-bearer-authentication.md) for configuration and key-cache behavior.
For a complete GitHub-backed deployment example, see
[GitHub authentication with Dex](../deploying/github-oidc.md).

## Chat Completions and Embeddings

### `POST /v1/chat/completions` · `POST /v1/completions` · `POST /v1/embeddings`

Agentic API owns the stateful agentic APIs; it does not own the Chat Completions or Embeddings contracts. These
paths are forwarded to `{LLM_API_BASE}` verbatim, so clients on those endpoints keep working when the gateway is
deployed as the entry point in front of an inference stack. Request and response bodies are relayed unchanged,
including `tools` and returned `tool_calls`; streaming responses are relayed as they arrive, and the upstream
status code and headers are preserved.

There is no state, no continuation, and no gateway tool loop on these routes: server-side tool execution is a
Responses and Messages capability. Inbound authentication, the request-body ceiling, and CORS apply as they do
to every other `/v1/*` route; the configured upstream credential is injected only when the caller supplies none.

## Models

### `GET /v1/models` · `GET /v1/models/{model}`

`GET /v1/models` returns the upstream model list unchanged. Codex clients, which send `client_version`, receive
it in the Codex catalog shape instead.

`GET /v1/models/{model}` returns one entry of that list exactly as the upstream wrote it. vLLM serves no
per-model route, so the gateway answers from `/v1/models` instead of forwarding the request. Model IDs that
contain `/`, such as `meta-llama/Llama-3.1-8B-Instruct`, work with the slash encoded or not. A model the upstream
does not list returns 404 with the error code `model_not_found`, and an upstream error is returned unchanged.
Both routes are stateless and use the same authentication as every other `/v1/*` route.

## Audio

### `POST /v1/audio/transcriptions` · `POST /v1/audio/translations`

Both routes forward the client's multipart upload to `{LLM_API_BASE}` verbatim, with its boundary and every form
field unchanged, and relay the upstream's answer as is: JSON or text according to `response_format`, and server-sent
events as they arrive when the form sets `stream=true`. The upload is streamed to the upstream rather than held in
memory, but the request-body ceiling still applies to the whole multipart body: an upload whose declared length is
over the limit is rejected with `413` before the upstream is called, and one sent without a declared length is cut
off with `413` once it passes the limit. Raise `max_request_body_size_bytes` for audio files above the 10 MiB
default. These routes are stateless pass-through, with the same authentication as every other `/v1/*` route.

## Responses

### `POST /v1/responses`

HTTP Responses requests always use the typed executor for input/tool normalization
and the tool loop. With `store:false` and no state IDs, clients supply the complete
item history; the gateway neither rehydrates nor persists the turn. A stored
`previous_response_id` or explicit `conversation` still supplies prior history,
but `store:false` does not store the response or append conversation items. An
unstored response ID cannot be retrieved or used for HTTP continuation.

Unmodeled top-level provider parameters, such as `chat_template_kwargs` and `top_k`,
are forwarded unchanged on each inference round. Their interpretation belongs to
the upstream provider. Known request fields still use the gateway's validation and
normalization rules. `store:false` is also forwarded to the upstream on every
inference round. Caller authentication and end-to-end routing headers are preserved;
hop-by-hop headers are filtered. An upstream HTTP error before the first streaming
event retains its status, body, and headers, including `Retry-After`.

Full upstream body compatibility remains a separate capability: typed execution
rejects unsupported file/audio input and text formats, prepares plaintext reasoning
for vLLM, and rejects encrypted-only reasoning that vLLM cannot replay. Reasoning
context and compatible opaque state support are tracked in
[#444](https://github.com/vllm-project/agentic-api/issues/444).

Multi-agent HTTP requests also support `store:false`. Append returned output items
unchanged to your input, then append new messages or matching client tool outputs.
The response ends with a root-attributed `compaction` item whose `encrypted_content`
carries the agent tree, private contexts, and pending client calls. Treat that content
as opaque; clients do not inspect or decode it. You may discard all items before the
latest compaction item, as in OpenAI's
[stateless compaction flow](https://developers.openai.com/api/docs/guides/compaction#server-side-compaction).
Keeping the complete history also works: the authenticated compaction supersedes its
prefix, and only subsequent input is applied as new work. The gateway writes no database
rows. A collaboration transcript without its final compaction item is rejected even
when `multi_agent` is omitted. Re-declare
the desired tools on each stateless request. For replay across gateway restarts
or replicas, configure the same `AGENTIC_MULTI_AGENT_REPLAY_KEY` everywhere (a
base64-encoded 32-byte secret); the default key lasts for the current process.
Stateless multi-agent execution uses automatic per-agent compaction; the gateway's
explicit `compaction_trigger` extension is unsupported in this mode.

Executor-backed requests accept at most 64 MCP server declarations and 128
discovered MCP tools. MCP discovery metadata shares the request's retained
response budget (`[responses] max_retained_bytes`, default 8 MiB) with upstream
rounds and gateway tool output.

#### Multi-agent

`multi_agent: {"enabled": true, "max_concurrent_subagents": 3}` makes the
gateway orchestrate subagents for one response and return attributed
`multi_agent_call`, `multi_agent_call_output`, and `agent_message` items.
`max_concurrent_subagents` defaults to 3 and must be positive. Multi-agent
requests reject `max_tool_calls` and reasoning summaries,
and run over HTTP JSON and SSE only; the WebSocket transport rejects them.
Continue a stored agent tree with `previous_response_id`, or replay the complete
item history with `store:false` as described above. See
[HTTP multi-agent execution](https://github.com/vllm-project/agentic-api/blob/main/ARCHITECTURE.md#http-multi-agent-execution).

#### Async client tools

`async: true` on a `function` or `custom` tool, including a function member of a
`namespace`, lets the model keep working after it calls that tool; the client returns
the output later on the original `call_id`, as in
[OpenAI async tool calling](https://developers.openai.com/api/docs/guides/async-tool-calling).
Calls to async tools carry `async: true`, in the response and in the streamed
`response.output_item.added`/`done` items. Requests that declare an async tool, or replay
an async call, always run through the executor; the llm-d split-execution backend rejects them.

The gateway matches the recorded OpenAI behavior:

- A follow-up request may omit the outputs of pending async calls; a synchronous call
  left unanswered is still rejected (`No tool output found for function call <id>.`).
- An async call accepts repeated and conflicting outputs, and the model receives each one.
  An output whose `call_id` matches no call returns `400`, `param: input`,
  `No tool call found for function call output with call_id <id>.`
- Whether a call is async is read from the call itself, so re-declaring the tool without
  `async` does not change a pending call.
- `async` on a namespace itself, on `shell`, on `tool_search`, or on a gateway-executed tool
  returns `400`, `code: unknown_parameter`, `param: tools[<index>].async`.
- Multi-agent mode with `parallel_tool_calls: true` and an async tool returns `400`,
  `code: unsupported_value`, `param: multi_agent`. The gateway defaults
  `parallel_tool_calls` to `false`, so only an explicit `true` conflicts.
- A response whose only client calls are async continues: the model answers in the same
  response. A synchronous client call in the same round ends the response, as before.

A model served by vLLM stops at a tool call and does not know `async`. The gateway
therefore runs another inference round with the call pending, and adds two hints to the
request it sends the model server: the async tool's description says that it runs in the
background, and a `developer` note follows each async call whose output has not arrived.
The note names the tool and `call_id` only when each is a plain identifier.
Without them the model tends to invent the pending result. The gateway does not store or
return the hints: responses to a request with async tools echo the client's own tool
declarations. The model's own reasoning output can still quote or paraphrase a hint.
After compaction, by any of its triggers, an async call whose output has not arrived stays
in the model context, so its output can still arrive. An output delivered in the request that
compacts is stored with the summarized history, so its call is no longer reported as pending,
and a further output for that call reaches the model after the call. This holds over HTTP
with `previous_response_id`, over a WebSocket session, and in multi-agent runs for calls the
gateway emitted; in multi-agent mode, a further output for an async call that the client
supplied in its own input is rejected with `400`. If the model
keeps calling async tools instead of answering, the gateway stops after two such rounds in a
row.

In multi-agent mode, a late async output is new information for the agent whose call it
answers. In a continuation request it starts a new root turn; in a live WebSocket run, only
once the root has finished its turn. A subagent that has finished its turn (not one that was
interrupted or failed) starts a new turn as soon as `max_concurrent_subagents` allows, and its
answer reaches its parent as mail; an idle root reads that mail at its next turn. An agent
that is still busy reads the output at its next turn. When the output arrives during the
agent's last round, that round ran without it, so the agent runs once more after the round
ends its turn (a subagent once a slot is free). For the root this means a streamed response can
contain a message with `phase: final_answer` followed by the root's next answer.

Differences from OpenAI:

- `async` is accepted for every served model; the gateway does not know which models
  support it natively, and the emulation above does not require support.
- When the model emits an async call after a gateway-executed tool call in the same
  round, the stream delivers the async call after that tool completes, keeping output
  indexes in order. Async calls before or without gateway calls stream immediately.

`prompt_cache_key` is forwarded unchanged on direct and executor-backed HTTP
requests, WebSocket requests, gateway tool rounds, automatic compaction,
standalone `/v1/responses/compact` requests, and summary inference requested by
a `compaction_trigger` input item.

- The key is scoped to the current request. A continuation using
  `previous_response_id` must send it again to remain in the same upstream
  cache group.
- Executor-backed requests omit an absent or `null` key. Direct HTTP requests
  preserve the original body, including an explicit `null`.
- The same request-scoped behavior applies when `/v1/responses/compact` loads
  history through `previous_response_id`. The key is not echoed in responses.

The configured upstream decides whether a request receives a cache hit.

Typed execution supports the mode/TTL subset of `prompt_cache_options` from the
[OpenAI SDK request contract](https://github.com/openai/openai-python/blob/b7a1990353baec896ee5e04b40b9f2dc2925a4f5/src/openai/types/responses/response_create_params.py),
checked on October 11, 2026. The object accepts optional `mode` (`"implicit"` or
`"explicit"`) and `ttl` (`"30m"`) members. Omitted members remain omitted, including
an empty object; the gateway inserts no defaults. Present members must be non-null.
The top-level absent or `null` object is treated as unset by typed execution.

Direct HTTP pass-through preserves the original request body, including options,
explicit `null`, prewarming, diagnostics, and future members. The upstream decides
whether to accept these values; the gateway does not apply its typed options parser
or capability switch to that route. `store: false` alone does not ensure pass-through:
a previous response ID, conversation, built-in tool, or another executor feature
still selects typed execution.

For typed execution, the configured upstream must implement these semantics for
every served model. Options are rejected by default on that path. After verifying
that contract, an operator can enable forwarding in `config.toml`:

```toml
[responses]
prompt_cache_options_supported = true
```

This switch is an operator assertion, not model-name detection or a capability probe.
Unsupported typed options return `400` with `param: "prompt_cache_options"` and
`code: "unsupported_parameter"` before history lookup, tool discovery, or inference.
Enabling the switch does not add typed support for prewarming, diagnostics, or future
members: `prewarm`, `comparison_response_id`, unknown members, invalid mode/TTL
values, and non-object options return `400` on executor routes.
`prompt_cache_options` and `prompt_cache_retention` are independent: both may be
provided and are forwarded without translating either into the other.

An accepted typed object is forwarded through HTTP JSON/SSE, WebSocket execution,
gateway tool rounds, multi-agent execution, automatic compaction,
`compaction_trigger` summary inference, and standalone `/v1/responses/compact`.
Options are request-scoped: a continuation restores items but does not inherit its
parent's top-level options. Send them again when needed. Existing
`prompt_cache_breakpoint` fields on modeled content parts are preserved; the
gateway does not insert or relocate breakpoints or replace explicit mode with
implicit mode. In the upstream explicit-mode contract, a request without explicit
breakpoints does not use prompt caching. Cache writes, TTL enforcement, and cache
reuse remain upstream responsibilities.

Typed Responses objects echo the current request's options in JSON, response-bearing
SSE/WebSocket events, and stored response snapshots. An absent or top-level `null`
request value is echoed as `null`; an empty object stays empty. This is request
metadata, not proof of upstream acceptance or a cache hit. Direct HTTP responses
remain upstream pass-through, and standalone compaction objects do not echo options.

| Path | Mode/TTL handling | Continuation behavior |
| --- | --- | --- |
| Direct HTTP JSON/SSE | Original options preserved; upstream validation; no gateway capability check | No gateway history resolution |
| Typed HTTP JSON/SSE and WebSocket | Object forwarded; absent/null omitted upstream; current request echoed in Responses objects | Top-level options are not inherited |
| Gateway tool rounds and multi-agent execution | Current request's options retained in every inference round | Current request scope |
| Automatic, trigger, and standalone compaction | Current request's options forwarded to summary inference | Loading a previous response does not inherit its options |

This matrix contributes to [#314](https://github.com/vllm-project/agentic-api/issues/314)
and [#330](https://github.com/vllm-project/agentic-api/issues/330). Qualification of all
content-part breakpoint lifecycles, model-specific capability profiles, and
upstream-reported cache diagnostics remain follow-ups.

`prompt_cache_retention` accepts the legacy `"in_memory"` and `"24h"` values.
An explicit value is forwarded through HTTP JSON/SSE, WebSocket execution,
gateway tool rounds, multi-agent execution, automatic compaction,
`compaction_trigger` summary inference, and `/v1/responses/compact`.
Continuations restore item history but do not inherit a previous request's
retention policy: send it again when needed. Missing or `null` values are omitted
by typed execution; direct HTTP preserves an explicit `null`. Invalid values
return `400` before inference. This is request forwarding; response-object
retention echoing is outside its scope.

Retention support depends on the configured upstream and model. The gateway
sets no retention default, manages no KV-cache lifetime, and does not translate
retention into `cache_salt` or `prompt_cache_options.ttl`. See the
[OpenAI prompt caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).
Forwarding is not a capability guarantee: the
[vLLM Responses request contract](https://github.com/vllm-project/vllm/blob/840c7c2f4bc72c7c55edb2dea6d23f9a9c9734e0/vllm/entrypoints/openai/responses/protocol.py)
checked on October 1, 2026 does not define `prompt_cache_retention`, so this field should not be
used to configure vLLM cache lifetime. Model-specific backend capability enforcement
remains a follow-up in
[#330](https://github.com/vllm-project/agentic-api/issues/330).

`service_tier` is also request-scoped. Direct and executor-backed HTTP requests,
WebSocket requests, gateway tool rounds, automatic compaction, standalone
`/v1/responses/compact` requests, and `compaction_trigger` summary inference
forward an explicit value unchanged. An absent or `null` value is not inherited
by a continuation. When the upstream reports the tier that actually served the
terminal inference round, that value is returned to the client and retained in stored response snapshots; it may
differ from the requested value. The gateway does not select a tier or silently
retry with another one. In multi-agent execution, the last root-agent inference
round determines the returned tier; child-agent tiers do not override it, and a
missing final root tier clears any earlier value.

#### `max_tool_calls`

`max_tool_calls` limits the built-in tool calls one response may dispatch: web
search, MCP calls, and code interpreter share one limit across every inference
round. Listing MCP tools and client-executed function, custom, and shell calls do
not count. A dispatched call counts even when it fails. The behavior matches the
OpenAI reference recordings under `crates/agentic-server-core/tests/cassettes/max_tool_calls`.

- The value must be an integer from 1 to 9223372036854775807. Other values return
  `400` with `param: "max_tool_calls"` and code `integer_below_min_value`,
  `integer_above_max_value`, or `invalid_type`, on direct and executor-backed HTTP
  requests and on WebSocket requests.
- A call over the limit is not executed. The model receives
  `{"error":"UserError: Reached tool call limit of N"}` as its tool call output, and later
  rounds of the response no longer offer gateway-executed built-in tools. One
  refused call stays in the output: the first refused `web_search_call` ends at
  `searching` or `code_interpreter_call` at `interpreting`, without a completed
  event. Refused MCP calls and every other refused call are omitted. The response
  still completes normally.
- Every response echoes `max_tool_calls` (`null` when unset). The value applies
  to one response and is not inherited through `previous_response_id`.
- Executor-backed requests enforce the limit in the gateway and do not forward
  it upstream; direct requests are passed through unchanged. Multi-agent
  requests reject it.

#### Unsupported request parameters

Typed execution cannot honour `background: true` or a reusable `prompt`
reference, so it rejects them with `400` and `type: "invalid_request_error"`
instead of silently dropping them. The check runs before history lookup, tool
discovery, storage, or inference. Direct HTTP pass-through preserves the original
request body, and the upstream decides whether to accept these members, whatever
their values.
`store: false` alone does not ensure pass-through: a previous response ID,
conversation, built-in tool, or another executor feature still selects typed
execution.

| Path | `background` | `prompt` |
| --- | --- | --- |
| Direct HTTP JSON/SSE | Forwarded unchanged; upstream validation | Forwarded unchanged; upstream validation |
| Typed HTTP JSON/SSE, WebSocket, and split-execution hydration | Absent, `null`, or `false` accepted and not forwarded; `true` returns `code: "unsupported_value"`, and any other value returns `code: "invalid_type"`, both with `param: "background"` | Absent or `null` accepted; any other value returns `code: "unsupported_parameter"`, `param: "prompt"` |

Background mode is tracked in
[#357](https://github.com/vllm-project/agentic-api/issues/357). This matrix
contributes to [#314](https://github.com/vllm-project/agentic-api/issues/314).

Typed execution still ignores other members the gateway does not model,
including `top_logprobs`, `user`, `safety_identifier`, `stream_options`, and vLLM
sampling extensions such as `top_k`, `min_p`, `seed`, and `stop`. Direct requests
forward them unchanged. Reporting effective parameters is tracked in
[#434](https://github.com/vllm-project/agentic-api/issues/434).

#### Prompt-cache usage compatibility

Cache-read usage is reported in `usage.input_tokens_details.cached_tokens`;
cache-write usage, when supplied by the upstream, is reported in
`usage.input_tokens_details.cache_write_tokens`. The wire names follow the
[OpenAI Responses usage contract](https://github.com/openai/openai-python/blob/becc1d20eed83c1b8d85e15dc131a372d9dc7813/src/openai/types/responses/response_usage.py),
checked on October 2, 2026. That SDK declares `cache_write_tokens` as a required
`int`. Accepting an absent or `null` counter is this gateway's compatibility
policy for other upstreams, rather than an allowance in that OpenAI contract.
Availability of these counters depends on the upstream.

HTTP Responses requests use typed response assembly and accounting for both `store` values.

| Path | Cache reads | Cache writes |
| --- | --- | --- |
| Typed HTTP JSON/SSE | Preserved for a single inference; accumulated across inference rounds | Accumulated when reported; explicit `0` is retained |
| WebSocket | Same typed accounting, scoped to the current response | Same typed accounting; a continuation does not carry over its parent's usage |
| Stored response retrieval | Returns the terminal response snapshot without inference | Preserves the terminal value or omission |

Typed execution treats an absent or `null` cache-write counter as unreported and
omits it when every round leaves it unreported. A round that reports the counter
contributes its value even when another round does not report it. The gateway
never derives cache writes from cache reads. Typed SSE accounting uses the
terminal response usage snapshot and counts each inference once; separate tool
inference rounds contribute separately. Reported `total_tokens` is preserved
independently rather than recomputed from the other counters. These counts do not establish a cache hit
rate or billing amount.

This matrix covers usage preservation under
[#330](https://github.com/vllm-project/agentic-api/issues/330) and
[#314](https://github.com/vllm-project/agentic-api/issues/314).
The mode/TTL forwarding matrix above covers request preservation with an operator
capability assertion. Full breakpoint qualification and model-specific backend
capability enforcement remain separate acceptance items.

### `GET /v1/responses/{response_id}`

Returns the terminal snapshot of a response created with `store: true`, including
status, usage, and that turn's output, without calling the upstream model.
Unknown IDs return `404`. Records created before snapshot storage, or through
history-only APIs, return `409`. Request-scoped MCP credentials are stripped
before storage. When OIDC is disabled and `OPENAI_API_KEY` is nonempty, callers
must send that key as a bearer token.

### `POST /v1/responses/compact`

Compacts direct input or a stored previous-response chain into a canonical
window of retained user messages plus one `compaction` item. See
[Responses compaction](../guides/responses-compaction.md) for request examples,
automatic threshold management, and the local plaintext limitation.

Multi-agent trees are unsupported on this endpoint, including those restored from
client-carried checkpoints or `previous_response_id`; these requests return HTTP 400
before inference. Multi-agent execution uses automatic per-agent compaction through
`/v1/responses`, matching OpenAI's
[multi-agent limitations](https://developers.openai.com/api/docs/guides/responses-multi-agent#limitations).

### `WS /v1/responses`

The same path accepts WebSocket upgrades for Codex-style Responses
continuations. Send one JSON text frame per turn:

```json
{
  "type": "response.create",
  "stream_id": "turn-1",
  "model": "test-model",
  "input": [{"type": "message", "role": "user", "content": "hi"}],
  "previous_response_id": "resp_optional",
  "store": true,
  "stream": true
}
```

The server normalizes the frame into the internal Responses request model and
uses the same core executor as HTTP, with connection-local continuation state. WebSocket replies are
JSON Responses stream events, including `response.created`,
`response.output_item.added`, `response.output_text.delta`, and
`response.completed`.

`store: false` keeps response state only in memory on the active connection. Each
lane retains its latest response, including `generate: false` prewarm responses,
so you can continue with `previous_response_id` without a database. After reconnecting,
replay the full item history or a compacted window; an unstored response ID returns
`400 previous_response_not_found`. With `store: true`, an uncached response can be
loaded from durable storage. Explicit `conversation` requests retain the durable
Conversations API behavior: `store:false` reads existing history without appending the new turn.

A failed ordinary same-lane continuation evicts its referenced cached parent. Failed forks
preserve the source lane's parent. Admission rejections (429) preserve existing
checkpoints and accepted queued work. Parent lookup happens when execution begins.

For a single-agent response on the current connection, send a steering update
while generation or tool execution is in progress:

```json
{
  "type": "response.steer",
  "previous_response_id": "resp_parent",
  "input": "Keep the work so far, but use Fahrenheit and summarize briefly."
}
```

Steering accepts text (including empty text) or a nonempty array of user messages
using the usual input text and image content types. Images require a vision-capable backend; file input
is not supported by the gateway. The target determines
the lane; do not add `stream_id` or response settings to the steering message.
`response.steer.accepted` returns a distinct steering ID. It acknowledges retained
input; the successor's `response.created` commits that input to a new response.
Multiple accepted updates are applied in acceptance order. Earlier output is
preserved, active output items and started tool work finish, and an interrupted
parent ends with `response.incomplete` and reason `steered`. Natural completion
may win that race. An eligible completed parent can also accept steering.

If the parent needs client-executed function or custom-tool output,
`response.steer.pending` lists the required call IDs. Submit a normal
`response.create` in the same lane with the parent's `previous_response_id` and
all required outputs. The server prepends the accepted steering; do not resend it.
An invalid output submission preserves pending steering so a corrected submission
can succeed. The explicit create uses its own settings and defaults. When no
client output is needed, the server creates a successor automatically using the
parent's effective settings and fresh response budgets.

`response.steer.failed` returns the original submitted input and a failure code;
accepted input that fails before successor creation also returns its steering ID.
After successor creation, later execution failure does not make that input
replayable. Control events can follow a parent's terminal event. Disconnect
discards unapplied steering, even for a stored parent; storage remains available
for ordinary continuation. Completed targets expire after 60 seconds and may be
evicted sooner under capacity pressure. Each target permits 32 queued updates;
the connection bounds accepted steering echoes at 12 MiB separately from ordinary
request and checkpoint budgets. Input-budget overflow returns `response.steer.failed`
with `too_many_pending_steers` and the submitted input. A separate 12 MiB budget bounds
retained settings, including targets without accepted steering. Settings are released
when the connection observes a newer `response.created` on the same lane, unless an
accepted batch still needs them. Targets whose settings exceed the remaining budget
continue normally but reject steering with `response_not_active`; ordinary continuation
is unaffected.
A separate 12 MiB allowance bounds failure echoes awaiting delivery; exhausting
failure-delivery capacity closes the connection.

Steering is local to generating, single-agent WebSocket responses. Multi-agent
responses, conversation-bound requests, compaction/context-management requests,
and unsupported client call kinds are rejected. Function, custom-tool, shell, and
client-executed tool-search calls report their required output stubs in
`response.steer.pending`; normal output validation still applies. It uses complete upstream item
lifecycle events as safe boundaries; a truncated or ambiguous stream cannot
establish that started work has settled. Backend model capability is not inferred
from its name. Non-generating prewarm responses and HTTP/SSE steering are outside
this interface; HTTP continues to support ordinary response continuation.

Set `stream_id` to a string containing 1 to 256 characters to multiplex
responses over one connection. Requests with different `stream_id` values can
run concurrently, while requests with the same value run first in, first out.
Every event for an accepted request, including an execution error event, echoes
its `stream_id`.
Requests that omit `stream_id` share a default first-in, first-out lane for
backward compatibility. A connection accepts at most 64 outstanding requests and
12 MiB of aggregate request data; additional requests receive a `429` error event
until capacity is available. A connection retains at most 128 lanes, including the
default lane; reconnect to start new lanes after that limit. Each retained checkpoint
is limited to 32,768 items and 16 MiB of serialized state, with a 32 MiB connection
budget covering cached, active-parent and prepared replacement checkpoints. Exceeding
a checkpoint budget returns 413 before writing response state. These limits do not
measure total process memory. Upstream SSE lines, the per-request retained response
budget, and outbound events follow the `[responses]` limits in `config.toml`
(`max_upstream_sse_line_bytes`, `max_retained_bytes`, and `max_stream_event_bytes`;
defaults 16 MiB, 8 MiB, and 16 MiB). Every outbound WebSocket event, including
`stream_id`, counts toward `max_stream_event_bytes`. Local `generate: false` requests validate both lifecycle events
before storing the response or emitting either event. OIDC identity expiry is
checked again when queued work starts, including work in the default lane;
expired work receives a tagged `invalid_token` event without reaching inference.

Invalid requests are returned as JSON WebSocket error events:

```json
{
  "type": "error",
  "stream_id": "turn-1",
  "status": 400,
  "error": {
    "message": "Previous response with id 'resp_missing' not found.",
    "type": "invalid_request_error",
    "code": "previous_response_not_found",
    "param": "previous_response_id"
  }
}
```

## Conversations

| Route | Operations |
| --- | --- |
| `/v1/conversations` | `POST` create, optionally with initial `items` |
| `/v1/conversations/{conversation_id}` | `GET` retrieve, `POST` update metadata, `DELETE` delete |
| `/v1/conversations/{conversation_id}/items` | `POST` add items, `GET` list items with cursor pagination and `order` |
| `/v1/conversations/{conversation_id}/items/{item_id}` | `GET` retrieve, `DELETE` delete |

Pass `conversation` to `POST /v1/responses` to continue a conversation; Responses
persistence appends each turn's items. Deleting an item detaches it from the
conversation without removing stored response history. Conversation creation and
its initial items commit atomically. These routes currently use the default
tenant rather than deriving ownership from the authenticated principal
([#107](https://github.com/vllm-project/agentic-api/issues/107)).

## Messages

### `POST /v1/messages` · `POST /v1/messages/count_tokens`

Anthropic Messages requests are forwarded to `{LLM_API_BASE}` with Anthropic
headers and body fields preserved. When a request declares a tool the gateway
owns, such as Claude's native web search and web fetch server tools (the
basic `web_search_20250305` and `web_fetch_20250910`, and their later versions
when `allowed_callers` includes `"direct"`), the gateway runs
the tool loop itself: it executes each gateway call, appends the result, and
streams only the client-visible content. Every upstream round must be a
complete, well-formed Messages stream; a truncated or malformed round ends the
response with an Anthropic `api_error` event before any gateway tool runs.
Messages requests are not persisted. See
[Claude Code integration](../design/claude-code-integration.md).
