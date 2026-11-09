# API Reference

See [Files, vector stores, and file search](file-search.md) for document ingestion,
retrieval, and the Responses `file_search` built-in tool.

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

## Rate limits

Per-caller request rate limiting is off unless `server.rate_limit_requests_per_minute` is set in the configuration
file, or `AGENTIC_RATE_LIMIT_REQUESTS_PER_MINUTE` or `--rate-limit-requests-per-minute` is given. A caller may send
`rate_limit_burst` requests at once (default: the per-minute rate) and then one request every
`60 / rate_limit_requests_per_minute` seconds.

Each request is charged to a caller the gateway can verify:

1. the OIDC token's issuer and subject, when OIDC is configured;
2. otherwise the connection's peer address. An IPv6 address counts as its /64 network, since one host commonly holds a
   whole /64.

A bearer token or `x-api-key` never chooses the bucket. Without OIDC the gateway does not verify them, and on executor
routes it may call the upstream with its own key, so a client could send a fresh made-up credential with every request.
Forwarded-for headers are not trusted either, since any client can set them. Behind a proxy or ingress, every request
arrives from the proxy's address, so all callers share one bucket. Enable OIDC to enforce per-caller limits against
untrusted clients. A WebSocket's `response.create` messages are charged to the caller of its upgrade request.

Every API route is limited except `/health`, `/ready`, and Swagger. A limited request never reaches the upstream.
It receives `429 Too Many Requests` with these headers:

| Header | Value |
| --- | --- |
| `retry-after` | Whole seconds until the caller's next request is allowed |
| `retry-after-ms` | The same wait in milliseconds |
| `x-ratelimit-limit-requests` | The configured requests per minute |
| `x-ratelimit-remaining-requests` | `0` |
| `x-ratelimit-reset-requests` | Time until the caller's full burst is available again, such as `1.5s` or `2m3s` |

The body uses the OpenAI format:

```json
{"error": {"message": "rate limit exceeded; retry after 2 seconds", "type": "rate_limit_error", "param": null, "code": "rate_limit_exceeded"}}
```

`/v1/messages` and `/v1/messages/count_tokens` answer in the Anthropic format, with the same `req_` identifier in the
`request-id` header:

```json
{"type": "error", "error": {"type": "rate_limit_error", "message": "rate limit exceeded; retry after 2 seconds"}, "request_id": "req_..."}
```

On the Responses WebSocket, the upgrade request is charged, and so is each `response.create` the client sends. A
limited create receives an `error` event with `status: 429` and the same `type` and `code`, and the connection stays
open. `response.steer` and `response.inject` act on a response that was already admitted and are not charged, nor are
continuations the gateway starts itself.

The limit is kept in each process's memory: with N replicas, a caller can send up to N times the configured rate. The
gateway tracks up to 100,000 callers. When that many are tracked, it forgets idle callers at most once a second, and new
callers share one bucket until a sweep frees room.

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

Multi-agent HTTP requests also support `store:false`. Send the complete history:
append returned output items unchanged to your input, followed by new messages or
matching client tool outputs. Re-declare `multi_agent` and the desired tools on each
request. Without `previous_response_id` or `conversation`, the gateway uses this
submitted input without loading stored history, and `store:false` skips persistence.
There is no separate checkpoint request or mandatory response-boundary compaction.
The coordinator routes public history and client tool outputs to their agent owners;
public collaboration items are not forwarded directly to the inference engine.
Encrypted `agent_message` contents remain opaque to clients. For returning these
messages across gateway restarts or replicas, configure the same
`AGENTIC_MULTI_AGENT_TRANSCRIPT_KEY` everywhere (a base64-encoded 32-byte secret);
the default key lasts for the current process. Automatic per-agent compaction remains
enabled; the explicit `compaction_trigger` extension is unsupported in this mode.

Executor-backed requests accept at most 64 MCP server declarations and 128
discovered MCP tools. MCP discovery metadata shares the request's retained
response budget (`[responses] max_retained_bytes`, default 8 MiB) with upstream
rounds and gateway tool output.

A client that replays a response's items (`store: false`) may send back its `tool_search_call` and
`tool_search_output` items. They are checked as a continuation's history is, so a malformed client search history gets
the same error with `store: false` as after a `previous_response_id`, and the model sees each pair as the function call
and output it saw live. Replayed `mcp_call` and `web_search_call` items are not converted yet. Sent inline, they reach
vLLM as public records, which it does not accept; sent by `id`, the stored record is left out of the model's input.
Continue such a response with `previous_response_id`.

#### Hosted tool search

`{"type": "tool_search"}` without `execution`, or with `execution: "server"`, makes the gateway run tool search
itself. Mark functions, namespace members, or whole `type: "mcp"` servers with `defer_loading: true`; the model sees
only their names and descriptions (an MCP server as `mcp_<server_label>`, described by `server_description`) until it
loads them. The model passes `{"paths": [...], "query"?: "..."}`: each path loads a deferred function, the deferred
members of a namespace, or an MCP server's tools, and a `query` keeps only the tools whose name or description contains
it, ignoring case. Unknown paths load nothing. Each search returns a `tool_search_call` and a `tool_search_output` with
`execution: "server"` and `call_id: null`, then the round continues with the loaded tools; tools loaded by an earlier
response are not offered again. Searches do not count toward `max_tool_calls`, and `parallel_tool_calls` may be true.
A deferred MCP server is listed at the start of a response as usual. As in the Responses API, if any declared MCP
server cannot list its tools (unreachable, an HTTP error, not an MCP server, or unauthorized), deferred or not, the
request fails: HTTP 424 with `type: "external_connector_error"`, `code: "http_error"`, and `param: "tools"` for JSON;
for a stream, the failed `mcp_list_tools` lifecycle, an `error` event, and `response.failed`. A continuation whose
history already holds a successful listing for that server keeps going, since the Responses API does not list it
again. An `allowed_tools` filter that matches none of the server's tools is not a failure: the listing is empty.
A `tool_choice` of type `mcp` is not supported (see [Unsupported request parameters](#unsupported-request-parameters)).
`execution: "client"` keeps client-executed search unchanged. Operator-configured `[mcp_servers]` entries may set
`defer_loading` and `description` as defaults; with `defer_loading = true`, a request that uses the server must declare
hosted tool search or set `defer_loading: false`, and the defaults appear in the response's `tools` echo.

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

#### Prompt-cache controls and service tiers

`prompt_cache_key` is forwarded unchanged on HTTP and WebSocket requests,
gateway tool rounds, automatic compaction, standalone `/v1/responses/compact`
requests, and summary inference requested by a `compaction_trigger` input item.

- The key is scoped to the current request. A continuation using
  `previous_response_id` must send it again to remain in the same upstream
  cache group.
- HTTP and WebSocket execution omit an absent or `null` key.
- The same request-scoped behavior applies when `/v1/responses/compact` loads
  history through `previous_response_id`. The key is not echoed in responses.

The configured upstream decides whether a request receives a cache hit.

Typed execution supports the mode/TTL subset of `prompt_cache_options` from the
[OpenAI SDK request contract](https://github.com/openai/openai-python/blob/b7a1990353baec896ee5e04b40b9f2dc2925a4f5/src/openai/types/responses/response_create_params.py),
checked on October 11, 2026. The object accepts optional `mode` (`"implicit"` or
`"explicit"`) and `ttl` (`"30m"`) members. Omitted members remain omitted, including
an empty object; the gateway inserts no defaults. Present members must be non-null.
The top-level absent or `null` object is treated as unset by typed execution.

Requests that set a cache options object are rejected by default. After verifying
mode and TTL support for a served model, enable forwarding for its exact ID in
`config.toml`:

```toml
[responses.models."qualified-model"]
prompt_cache_options_supported = true
```

The model-specific setting overrides `[responses] prompt_cache_options_supported`.
Models without an override use that deployment default, which is `false` unless
configured. Qualify all models using the default before setting it to `true`.
Setting an options object for a model with forwarding disabled returns `400`
with `param: "prompt_cache_options"` and `code: "unsupported_parameter"` before
history lookup, tool discovery, or inference. The typed parser rejects `prewarm`,
`comparison_response_id`, unknown members, invalid mode or TTL values, and
non-object options even when forwarding is enabled.
`prompt_cache_options` and `prompt_cache_retention` are independent. When the
policy allows both, the gateway forwards both as provided.

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
request value is echoed as `null`, and an empty object stays empty. The echo
describes the request. Standalone compaction objects omit this metadata.

| Path | Mode/TTL handling | Continuation behavior |
| --- | --- | --- |
| HTTP JSON/SSE and WebSocket | Object forwarded when enabled; absent/null omitted upstream; current request echoed in Responses objects | Top-level options are not inherited |
| Gateway tool rounds and multi-agent execution | Current request's options retained in every inference round | Current request scope |
| Automatic, trigger, and standalone compaction | Current request's options forwarded to summary inference | Loading a previous response does not inherit its options |

`prompt_cache_retention` accepts the legacy `"in_memory"` and `"24h"` values.
Allowed values are forwarded through HTTP JSON/SSE, WebSocket execution,
gateway tool rounds, multi-agent execution, automatic compaction,
`compaction_trigger` summary inference, and `/v1/responses/compact`.
Continuations restore item history but do not inherit a previous request's
retention policy, so send it again when needed. Execution omits missing or
`null` values and rejects invalid values with `400` before inference.

The upstream manages cache lifetime. The gateway forwards retention separately
from `cache_salt` and `prompt_cache_options.ttl`. The pinned
[vLLM Responses request contract](https://github.com/vllm-project/vllm/blob/840c7c2f4bc72c7c55edb2dea6d23f9a9c9734e0/vllm/entrypoints/openai/responses/protocol.py)
checked on October 1, 2026 lacks the legacy `prompt_cache_retention` field.
Set `prompt_cache_retention_supported = false` on a model policy to reject the
field before inference when the deployment cannot honor it. See the
[OpenAI prompt caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).

`service_tier` is also request-scoped. HTTP and WebSocket requests,
gateway tool rounds, automatic compaction, standalone
`/v1/responses/compact` requests, and `compaction_trigger` summary inference
forward an explicit value unchanged. An absent or `null` value is not inherited
by a continuation. When the upstream reports the tier that actually served the
terminal inference round, the gateway returns that value to the client and retains
it in the stored response snapshot. A tier rejection follows the ordinary
upstream error path. In multi-agent execution, the last root-agent inference
round determines the returned tier, and a missing final root tier clears any
earlier value.

#### Model capability policies

The gateway selects an optional `[responses.models."<served-model-id>"]` policy
by exact match with the current request's `model`. This also applies when
continuing a response or conversation, including a WebSocket continuation.
These policies are separate from the `[models]` entries used by the Codex catalog.

The following policy follows the published Responses contract for the public
`gpt-6-astra` model ID. The
[model page](https://developers.openai.com/api/docs/models/gpt-6-astra) and
[migration guide](https://developers.openai.com/api/docs/guides/latest-model#update-api-and-model-parameters)
were checked on November 8, 2026. The model page lists the public ID without a
separate dated snapshot.

```toml
[responses.models."gpt-6-astra"]
sampling_supported = false
logprobs_supported = false
reasoning_efforts = ["low", "medium", "high", "xhigh", "max"]
```

| Policy field | Request admission |
| --- | --- |
| `sampling_supported = false` | Rejects non-null `temperature` and `top_p`. |
| `logprobs_supported = false` | Rejects non-null `top_logprobs` and `message.output_text.logprobs` in `include`. |
| `reasoning_efforts` | Allows explicitly set `reasoning.effort` values from this list. |
| `service_tiers` | Allows explicitly set `service_tier` values from this deployment's list. |
| `prompt_cache_options_supported` | Enables or disables forwarding the qualified mode/TTL subset for this model, overriding the deployment default. |
| `prompt_cache_retention_supported = false` | Rejects a non-null legacy `prompt_cache_retention` value. |

When a model policy or field is omitted, the upstream continues to validate the
affected parameter, subject to the deployment default for `prompt_cache_options`
and the gateway's typed request parser. An empty allowlist rejects every
explicit value for that field. Set `service_tiers` only for known
deployment restrictions. The upstream determines actual account and regional
availability and returns the tier it served.

The checks run before history lookup, tool discovery, and inference for HTTP
JSON/SSE and WebSocket. A violation returns HTTP `400` or a WebSocket `error`
event with `type: "invalid_request_error"` and the affected `param`. The code is
`unsupported_value`, except that disabled `prompt_cache_options` uses
`unsupported_parameter`. Standalone compaction applies the configured checks
for `prompt_cache_options`, `prompt_cache_retention`, and `service_tier` using
the model in the compaction request.

For a request that passes local admission, an upstream HTTP rejection before
streaming starts retains the upstream status and body. After a stream starts,
clients receive the failure through the transport's existing error path.
WebSocket errors retain the upstream explanation in the gateway's error envelope.

#### Content-part cache breakpoints

The pinned SDK defines `prompt_cache_breakpoint: {"mode": "explicit"}` on
[input text](https://github.com/openai/openai-python/blob/b7a1990353baec896ee5e04b40b9f2dc2925a4f5/src/openai/types/responses/response_input_text_param.py),
[images](https://github.com/openai/openai-python/blob/b7a1990353baec896ee5e04b40b9f2dc2925a4f5/src/openai/types/responses/response_input_image_param.py), and
[files](https://github.com/openai/openai-python/blob/b7a1990353baec896ee5e04b40b9f2dc2925a4f5/src/openai/types/responses/response_input_file_param.py).
These are content-part fields, distinct from the request-scoped mode/TTL options.
The gateway preserves them as content extensions; upstream support and validation
remain required. It also forwards explicit `null` and future breakpoint values
unchanged on supported parts, without interpreting them as valid cache policies.

| Path or transformation | Breakpoint handling |
| --- | --- |
| Typed HTTP JSON/SSE | Text/image part boundaries and order are preserved; no boundary is inserted on unmarked parts |
| Stored and WebSocket history | Boundaries remain attached to restored content, including after a durable WebSocket reconnect or local `generate: false` completion |
| Image defaults and client tool normalization | Adding image `detail: "auto"` or lowering custom outputs to function outputs preserves content extensions, including structured text/image/file tool outputs |
| Gateway tool rounds and multi-agent history forks | Original text/image boundaries remain in every inference round that carries those items |
| Standalone, automatic, and trigger compaction | Summary inference receives original boundaries; retained user parts keep theirs; boundaries on replaced content are not moved to the generated summary |
| Inline UTF-8 text message files | Supported base64 `file_data` becomes model-facing `input_text` in place, retaining its cache breakpoint; canonical and stored history keep the original `input_file` |
| Unsupported message files or unknown content | File IDs/URLs, unsupported non-text files, invalid base64/UTF-8, and unknown content types are rejected before inference or storage |

Text-file inlining prefixes decoded text with `File: <filename>` when a filename
is present. Only the shared `prompt_cache_breakpoint` extension is carried onto the
new text part; file-only extension fields stay on the original file in history.
This preserves a boundary at the same content position, without claiming identical
provider tokenization or cache reuse before and after conversion.

Restoring a marked content part does not inherit its parent's top-level
`prompt_cache_options`: repeat those options on a new request when needed.
Explicit mode without breakpoints stays explicit and does not gain an implicit
fallback. Compaction may replace the marked content; replay the complete compacted
window rather than assuming the old prefix is still present. The gateway does not
recalculate cache boundaries, move them to summaries, or establish cache hits.
This matrix is covered with deterministic mock-upstream tests, without claims
about live-model reuse or TTL enforcement.

#### `max_tool_calls`

`max_tool_calls` limits the built-in tool calls one response may dispatch: web
search, MCP calls, and code interpreter share one limit across every inference
round. Listing MCP tools and client-executed function, custom, and shell calls do
not count. A dispatched call counts even when it fails. The behavior matches the
OpenAI reference recordings under `crates/agentic-server-core/tests/cassettes/max_tool_calls`.

- The value must be an integer from 1 to 9223372036854775807. Other values return
  `400` with `param: "max_tool_calls"` and code `integer_below_min_value`,
  `integer_above_max_value`, or `invalid_type`, on HTTP and WebSocket requests.
- A call over the limit is not executed. The model receives
  `{"error":"UserError: Reached tool call limit of N"}` as its tool call output, and later
  rounds of the response no longer offer gateway-executed built-in tools. One
  refused call stays in the output: the first refused `web_search_call` ends at
  `searching` or `code_interpreter_call` at `interpreting`, without a completed
  event. Refused MCP calls and every other refused call are omitted. The response
  still completes normally.
- Every response echoes `max_tool_calls` (`null` when unset). The value applies
  to one response and is not inherited through `previous_response_id`.
- The gateway enforces the limit locally. Multi-agent requests reject it.

#### Unsupported request parameters

Typed execution rejects `background: true` and reusable `prompt` references with
`400` and `type: "invalid_request_error"`. The check runs before history lookup,
tool discovery, storage, or inference for both `store` values.

| Path | `background` | `prompt` | `tool_choice` of type `mcp` |
| --- | --- | --- | --- |
| Typed HTTP JSON/SSE, WebSocket, and split-execution hydration | Absent, `null`, or `false` accepted and not forwarded; `true` returns `code: "unsupported_value"`, and any other value returns `code: "invalid_type"`, both with `param: "background"` | Absent or `null` accepted; any other value returns `code: "unsupported_parameter"`, `param: "prompt"` | `code: "unsupported_value"`, `param: "tool_choice"` |

Background mode is tracked in
[#357](https://github.com/vllm-project/agentic-api/issues/357). This matrix
contributes to [#314](https://github.com/vllm-project/agentic-api/issues/314).

Unmodeled provider members such as `user`, `safety_identifier`, `stream_options`,
`top_k`, `min_p`, `seed`, and `stop` are forwarded unchanged. A request that
passes the model capability policy also forwards `top_logprobs` unchanged.
Reporting effective parameters is tracked in
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

#### Responses capability matrix

This is the Responses caching and service-tier portion of the
[#314 capability contract](https://github.com/vllm-project/agentic-api/issues/314)
and the acceptance matrix for
[#330](https://github.com/vllm-project/agentic-api/issues/330).
Both HTTP storage modes use the executor. The differential tests compare
`store: false` and `store: true` across the current execution paths.

| Control | Supported | Transformed | Rejected | Regression evidence |
| --- | --- | --- | --- | --- |
| `prompt_cache_key` | Forwarded on HTTP JSON/SSE, WebSocket, tool rounds, and compaction. | Absent and null values are omitted upstream. Continuations use the current request's value. | Invalid types. | [#347](https://github.com/vllm-project/agentic-api/pull/347), [#412](https://github.com/vllm-project/agentic-api/pull/412) |
| `prompt_cache_options` | Enabled models forward `implicit`/`explicit` and `30m`. Response objects echo the current request. | Absent and null values are omitted upstream and echoed as null. Continuations use the current request's options. | Disabled models, invalid values, unknown members, and unsupported TTLs. | [#436](https://github.com/vllm-project/agentic-api/pull/436) and model policy tests |
| `prompt_cache_retention` | Forwards `in_memory` and `24h` independently of `cache_salt` and cache options. | Absent and null values are omitted upstream. Continuations use the current request's retention. | Invalid values and models whose policy disables legacy retention. | [#413](https://github.com/vllm-project/agentic-api/pull/413) and model policy tests |
| Content breakpoints | Preserved on supported parts through item history, tool rounds, and compaction. Explicit mode stays explicit. | Inline UTF-8 files become model-facing text with the same breakpoint. Stored history keeps the file item. | Unsupported message files and content types. | [#467](https://github.com/vllm-project/agentic-api/pull/467) |
| `service_tier` | Explicit request values reach the upstream. The final root inference supplies the actual tier in the response and stored snapshot. | Absent and null values are omitted upstream. Continuations use the current request's tier. | Tiers excluded by a model's deployment policy, plus upstream account/region errors. | [#389](https://github.com/vllm-project/agentic-api/pull/389) and model policy tests |
| Sampling, logprobs, and `reasoning.effort` | Accepted values reach the upstream. Each model uses its own policy. | Typed null values for sampling and effort are treated as unset. | Values and logprobs selections disallowed by the matching model policy. | Model policy tests use the documented `gpt-6-astra` controls and compare another served model. |
| Cache-read/write usage | Preserves upstream reads and reported writes, including explicit zero, through streaming and stored retrieval. | Aggregates terminal usage across inference rounds. Missing writes remain omitted. | — | [#421](https://github.com/vllm-project/agentic-api/pull/421) |

The deterministic model policy tests exercise the published `gpt-6-astra`
parameter rules and upstream error propagation. Live model acceptance, cache
reuse, and regional tier availability depend on the upstream deployment.

### `GET /v1/responses/{response_id}`

Returns the terminal snapshot of a response created with `store: true`, including
status, usage, and that turn's output, without calling the upstream model.
Unknown IDs return `404`. Records created before snapshot storage, or through
history-only APIs, return `409`. Request-scoped MCP credentials are stripped
before storage. When OIDC is disabled and `OPENAI_API_KEY` is nonempty, callers
must send that key as a bearer token.

### `DELETE /v1/responses/{response_id}`

Deletes a stored response and returns
`{"id": "<response_id>", "object": "response.deleted", "deleted": true}`.
Unknown or already deleted IDs return `404`. The deleted response can no
longer be retrieved or used as a durable `previous_response_id`.

History item rows are retained: descendant responses and conversations may
still reference them. Descendants remain retrievable and continuable, but
response-chain descendants exclude the deleted response's inherited context
from their input listings and subsequent inference. Conversation snapshots
retain their recorded context. These behaviors are covered by OpenAI and
gateway lifecycle recordings, including continuation after parent deletion.
Existing foreign keys clear pointers to the deleted response, including a
conversation's `latest_response_id`; deletion does not choose an older turn
as replacement state. Authentication matches response retrieval.

Deletion does not invalidate a live WebSocket session's cached checkpoint.
Such a session may still continue without storage; persisting a child of the
deleted response fails. New connections resolve durable IDs through storage.

### `GET /v1/responses/{response_id}/input_items`

Lists the stored context for a response: inherited history, including earlier
outputs, followed by this turn's inputs. This response's appended outputs are
excluded. The endpoint uses the existing history IDs, item records, and saved
response snapshot; it does not add metadata or change persistence.

Pagination uses the Conversation API query parameters: `limit` (1–100,
default 20), `order` (`asc` or `desc`, default `desc`), and `after` (a public
item ID; unknown cursors return `404`). A repeated public ID resumes after
its last occurrence in the requested order. `include` or `include[]` accepts
`message.output_text.logprobs`, which stored items already contain; other
expansions return `400`, as on conversation item listing. Malformed or repeated
scalar query parameters return `400`.

Unknown or unstored responses return `404`. Legacy responses without a saved
payload return `409`, as on response retrieval. Authentication matches retrieval.

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
checkpoints and accepted queued work. Model capability rejections also preserve the
parent checkpoint, so a client can retry with supported settings. Parent lookup happens
when execution begins.

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
returns the content of every round. Every upstream round must be a
complete, well-formed Messages stream; a truncated or malformed round ends the
response with an Anthropic `api_error` event before any gateway tool runs.
Messages requests are not persisted. See
[Claude Code integration](../design/claude-code-integration.md).

Each native web search or web fetch call the gateway runs reaches the client as
Anthropic returns it: a `server_tool_use` block, whose id is `srvtoolu_gw_`
followed by the upstream `tool_use` id, then the matching
`web_search_tool_result` or `web_fetch_tool_result` after the round's other
blocks. The `server_tool_use` input is the call as the model made it, in the
gateway's search schema (`query`, or `queries` for a batch). `usage.server_tool_use`
counts the searches (one per query) and fetches that ran. A call through an
operator alias, a plain function named `web_search`, or a call in a round the
gateway does not run, such as one beside a client tool call, stays hidden.

Send these blocks back unchanged. The gateway turns them into the
`tool_use` / `tool_result` rounds the upstream saw, with the same ids and the
same result text, on `/v1/messages` (with or without a gateway tool) and on
`/v1/messages/count_tokens`. A search result's page text is sealed in its
`encrypted_content`; search content that was altered or not sealed by the
gateway, and a gateway block that is unpaired or repeated, are refused with
HTTP 400 `invalid_request_error`. A fetched page is not sealed and replays as
sent. A round that also ran a hidden call replays without it, since the client
never saw that call. Search results sealed under
another key, such as by a process without `AGENTIC_MESSAGES_RESULT_KEY` before
a restart, reach the model as a note that they are no longer available. A
`web_fetch_tool_result` also carries the page's `content_type` and `truncated`
flag, and a refusal its `message`, so the model reads the same text again.

### Message threads


Requests without `thread` retain the existing transparent proxy or gateway tool
loop behavior. To keep conversation history locally, use Anthropic's
[message threads](https://platform.claude.com/docs/en/build-with-claude/message-threads)
contract and send `anthropic-beta: message-threads-2026-08-12` on every turn:

```bash
curl http://localhost:8080/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-beta: message-threads-2026-08-12' \
  -d '{"model":"local-model","max_tokens":128,"system":"Keep replies short.",
       "messages":[{"role":"user","content":"Remember the number 42."}],
       "thread":{"type":"create"}}'
```

Continue using the returned message `id`, with only the new messages:

```bash
curl http://localhost:8080/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-beta: message-threads-2026-08-12' \
  -d '{"model":"local-model","max_tokens":128,
       "messages":[{"role":"user","content":"Which number?"}],
       "thread":{"type":"continue","previous_message_id":"msg_REPLACE_ME"}}'
```

The contract is implemented from the public Anthropic documentation. The checked-in
recordings capture vLLM inference and this gateway's public responses; they are not
reference captures from Anthropic's message-threads beta. Exact error-envelope parity,
null-versus-omitted fixed-field equivalence, paused-turn rejection, and continued
stream `message_start` IDs have local regression coverage but no Anthropic reference
capture. Treating omitted and null optional fixed fields as equal is the gateway's
current policy pending that reference verification.

The gateway stores thread state in the configured SQLite or PostgreSQL database,
restores the full conversation for the stateless upstream, and consumes this beta locally. It preserves Messages content, including images, thinking
signatures, client `tool_use`/`tool_result`, and hidden gateway tool rounds. The
Messages thread request, checkpoint, and error types are independent of Responses
items and response sessions. Thread checkpoints use their own `message_threads`
table and preserve the Anthropic object model.

Omit `system` and `tools` entirely on continuation, including `null`. Keep `model`,
`tool_choice`, `thinking`, `mcp_servers` (except authorization tokens),
`context_management`, `output_config`, container skills, top-level `cache_control`,
and the beta set fixed. Send MCP and container parameters on each applicable
request; only the presence of `fallbacks` is fixed. Generation settings such as
`max_tokens`, `stop_sequences`, and `stream` can change.

The gateway provides a Claude Code compatibility extension when the beta set
includes `claude-code-20250219` from thread creation onward. These continuations
may repeat `system` and `tools` only when their JSON values equal the stored
values. Changed prompts or tool definitions return `thread_fingerprint_mismatch`;
explicit `null` remains invalid. The CLI's reserved, single-line
`x-anthropic-billing-header:` text block is removed from `system` before storage,
comparison, and inference. A continuation containing only that attribution block
inherits the stored prompt. Blocks with extra fields, control characters, or
malformed attribution syntax remain ordinary prompt content. This is a gateway
extension for Claude Code's wire behavior; ordinary thread requests retain the
create-only rules above.

Each message can be continued once. Simultaneous continuations return
`400 thread_continue_in_flight`; an already consumed ID returns
`400 thread_already_continued`. Fixed-field changes return
`400 thread_fingerprint_mismatch`. These codes appear in `error.details.error_code`.
Use the same credentials and workspace throughout a thread; with OIDC, ownership
also includes the verified issuer and subject.

Set `stream: true` for SSE. Continue from the ID in `message_start` only after
`message_stop`. Failed or incomplete streams publish no checkpoint and release the
parent reservation. A `pause_turn` continuation requires `messages: []`.
Refusals, fallback-model responses, and token-truncated tool calls produce no
continuable checkpoint.

Thread checkpoints survive gateway restarts and can be continued by another gateway
instance sharing the same database and credentials. Parent consumption and child
checkpoint insertion commit in one transaction before the JSON response or SSE
`message_stop` reaches the client. A failed write returns an error and leaves the
parent retryable. Threaded requests require configured persistence; there is no
silent memory-only fallback.

Threads retain the five-minute lifetime after each completed response. The server
polls database expiry and renews active continuation leases every ten seconds.
Leases expire after sixty seconds without renewal, so an interrupted gateway cannot
lock a thread indefinitely. A replacement lease fences stale completions from the
previous process. Graceful shutdown joins cancellation cleanup operations.

Each serialized checkpoint and each threaded SSE request has a 32 MiB budget.
An oversized checkpoint fails before committing the new message. Thread history is
stored in the database, so database retention and access controls apply. There is
no list, read, or delete thread API.

Keep a client copy of the conversation. On `404 thread_not_found`, replay that
copy with `thread: {"type":"create"}`, `system`, and `tools`. Remove older cache
breakpoints to stay within the four-breakpoint limit. Token counting rejects
`thread`; send the full conversation without it. Thread requests also reject
compaction and context editing other than `clear_thinking_20251015` with
`keep: "all"`.
