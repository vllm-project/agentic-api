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

## Chat Completions

### `POST /v1/chat/completions` · `POST /v1/completions`

Agentic API owns the stateful agentic APIs; it does not own the Chat Completions contract. Both paths are
forwarded to `{LLM_API_BASE}` verbatim, so clients on those endpoints keep working when the gateway is deployed
as the entry point in front of an inference stack. Request and response bodies are relayed unchanged, including
`tools` and returned `tool_calls`; streaming responses are relayed as they arrive, and the upstream status code
and headers are preserved.

There is no state, no continuation, and no gateway tool loop on these routes: server-side tool execution is a
Responses and Messages capability. Inbound authentication, the request-body ceiling, and CORS apply as they do
to every other `/v1/*` route; the configured upstream credential is injected only when the caller supplies none.

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
loaded from durable storage. Explicit `conversation_id` requests retain the durable
Conversations API behavior: `store:false` reads existing history without appending the new turn.

A failed same-lane continuation evicts its referenced cached parent. Failed forks
preserve the source lane's parent. Admission rejections (429) preserve existing
checkpoints and accepted queued work. Parent lookup happens when execution begins.

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
`stream_id`, counts toward `max_stream_event_bytes`. Multi-agent requests are
not supported on this transport. Local `generate: false` requests validate both lifecycle events
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
owns, such as the native `web_search_20250305` and `web_fetch_20250910`
server tools, the gateway runs
the tool loop itself: it executes each gateway call, appends the result, and
streams only the client-visible content. Every upstream round must be a
complete, well-formed Messages stream; a truncated or malformed round ends the
response with an Anthropic `api_error` event before any gateway tool runs.
Messages requests are not persisted. See
[Claude Code integration](../design/claude-code-integration.md).
