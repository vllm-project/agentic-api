# Configuration

The default database is `~/.agentic-api/agentic_api.db`, so running an installed binary does not create state in the
current directory. Set `AGENTIC_API_HOME` to an absolute directory to move both the default database and user
configuration, or set `DATABASE_URL`/`--db-url` to select a different database.

On startup, Agentic API creates `~/.agentic-api` and loads `~/.agentic-api/config.toml` when that file exists. On the
first invocation with a resolved LLM base URL, a missing config file is generated from the effective
`--llm-api-base`/`LLM_API_BASE` and non-secret tool settings. It records the name of the web-search API-key environment
variable, never its value. The generated file is group-readable but not group-writable on Unix (so a container restart
under a different arbitrary UID sharing the same group can still read it) and is never overwritten on later
runs. CLI arguments and process environment variables take precedence over file settings. A standalone server can
therefore be started with just `agentic-server` after creating a config like this:

```toml
llm_api_base = "http://127.0.0.1:5050"
# database_url = "postgresql://agentic-api@localhost/agentic_api"

[web_search]
# Search backend for the gateway-owned web_search tool: "you" (default), "brave", "tavily", "serply", "keenable", or "searxng".
provider = "you"
base_url = "https://ydc-index.io"
api_key_env = "YOU_API_KEY"
# Concurrent provider requests inside one batched web-search call; unset uses
# the provider default (Brave: 1; You.com, Tavily, Serply, Keenable and SearXNG: max_concurrent_gateway_calls).
# max_concurrent_queries = 1

[web_fetch]
# Gateway-executed page fetches for Claude's native web_fetch tool on /v1/messages.
# enabled = true
# Allow fetches of private, loopback, and other non-public addresses (off by default).
# allow_private_networks = false
# Download ceiling per fetch in bytes and time ceiling per fetch in seconds.
# max_response_bytes = 10485760
# timeout_secs = 20

[mcp]
allowed_hosts = ["mcp.example.com"]

[server]
# Maximum serialized request size in bytes for HTTP bodies and WebSocket
# messages and frames. Must be greater than zero.
max_request_body_size_bytes = 10485760
# Optional per-caller request rate limit; unset disables it.
# rate_limit_requests_per_minute = 600
# rate_limit_burst = 600

[tools]
# Upper bound for gateway-owned calls within one Responses or Messages round and for
# provider requests inside a batched web-search call.
# Must be greater than zero.
max_concurrent_gateway_calls = 5

[responses]
# Cumulative logical retained response data across all rounds of a turn.
# Defaults to 8 MiB (8388608).
max_retained_bytes = 8388608

# Maximum size for a single upstream JSON response body.
# Defaults to 16 MiB (16777216).
max_upstream_json_bytes = 16777216

# Maximum size for a single upstream SSE line.
# Defaults to 16 MiB (16777216).
max_upstream_sse_line_bytes = 16777216

# Maximum size for a single serialized outbound stream event.
# Defaults to 16 MiB (16777216).
max_stream_event_bytes = 16777216

[models."Qwen/Qwen3-VL-8B-Instruct"]
# Input modalities to advertise for this served model ID.
# Accepted values are "text" and "text" + "image"; text is always required.
input_modalities = ["text", "image"]

[models."zai-org/GLM-5.3-Flash"]
# Reasoning effort levels to offer for this served model ID, in the order Codex lists them.
# Accepted values are "low", "medium", "high", "xhigh", and "max".
supported_reasoning_levels = ["low", "high", "max"]

[mcp_servers.counter]
url = "https://mcp.example.com/mcp"
allowed_tools = ["tool_1_name", "tool_2_name"]
require_approval = "never"
# Optional: defer this server's tools behind hosted tool search unless a request
# sets `defer_loading`, and describe it in the search catalog when a request
# omits `server_description`. Requests that use a deferred server must then
# declare `{"type": "tool_search"}` (hosted), or set `defer_loading = false`.
# defer_loading = true
# description = "Counter tools"
```

`[models."<served-model-id>"]` declares capabilities the gateway cannot infer. `input_modalities` resolves in this
order: an explicit override here, then `capabilities: ["image"]` from the upstream `/v1/models` entry, then a
conservative text-only fallback. Capabilities are never guessed from a model name, so a vision model served without
capability metadata needs this override. An explicit `["text"]` wins over upstream image metadata, which pins a model
to text. Unknown modality names, empty lists, duplicates, and image-only lists are rejected at startup with the
offending file and line.

This matters because Codex reads its local model catalog and strips image content from a request before sending it
when the catalog says the model is text-only, so a missing override blocks image input even against a vision-capable
upstream.

`supported_reasoning_levels` lists the reasoning effort levels Codex offers for that served model, in the order the
catalog advertises them. A model without the setting keeps `["low", "medium", "high"]`, so existing catalogs are
unchanged. Levels use the shared effort vocabulary (`low`, `medium`, `high`, `xhigh`, and `max`); unknown level names,
empty lists, and repeated levels are rejected at startup with the offending file and line. Each level's Codex-facing
description comes from a fixed per-level table rather than from the configuration file.

`max_request_body_size_bytes` bounds the serialized request the gateway accepts on `/v1/responses`,
`/v1/responses/compact`, `/v1/conversations`, the Anthropic Messages endpoints, the Responses WebSocket, and the
pass-through routes (Chat Completions, Completions, Embeddings, and the audio uploads). It counts
encoded bytes (JSON overhead, replayed conversation history, and base64 image attachments included) and is unrelated
to the model's token context limit, so raising it does not raise what the upstream will accept. It defaults to
10 MiB (10485760); inline base64 attachments cost roughly a third more than the source image, so conversations that
replay several images may need a higher value, and an audio upload counts its whole multipart body, so larger audio
files need one too. Oversized HTTP requests are answered with `413 Payload Too Large`;
oversized WebSocket messages and frames are rejected by the transport, which closes the connection before any JSON is
parsed. Order of precedence is `--max-request-body-size-bytes`, then `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES`, then this
file setting.

`rate_limit_requests_per_minute` turns on a per-caller request rate limit on every API route and on each WebSocket
`response.create`; it is off when unset. `rate_limit_burst` sets how many requests a caller may send at once
and defaults to the per-minute rate. A caller over its limit receives `429` with `retry-after`. The limit is per
process, so N replicas allow N times the rate. Order of precedence is `--rate-limit-requests-per-minute` and
`--rate-limit-burst`, then `AGENTIC_RATE_LIMIT_REQUESTS_PER_MINUTE` and `AGENTIC_RATE_LIMIT_BURST`, then these file
settings. Callers are told apart by their OIDC identity, otherwise by peer address, never by a credential the
gateway does not verify. Behind a proxy every request shares the proxy's address, so enable OIDC for per-caller limits.
See [Rate limits](../api/index.md#rate-limits) for how callers are identified.

`api_key_env` names the process environment variable containing the web-search credential; it does not contain the
credential itself. When it is unset, the selected provider's conventional variable is read (`YOU_API_KEY`,
`BRAVE_API_KEY`, or `TAVILY_API_KEY`). Newly generated files leave `api_key_env` unset so changing providers also changes the default
credential variable. `AGENTIC_WEB_SEARCH_PROVIDER`, `AGENTIC_WEB_SEARCH_BASE_URL`, `AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES`,
`AGENTIC_MCP_ALLOWED_HOSTS`, `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES`, `AGENTIC_RATE_LIMIT_REQUESTS_PER_MINUTE`,
`AGENTIC_RATE_LIMIT_BURST`, and `AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS` can override their typed file settings; `YOU_API_BASE_URL` is still honored as the endpoint override when the provider is
`you`. The concurrency values are sliding-window upper bounds; handlers may further serialize calls to the same tool
name. The MCP allowlist is used only for request-declared remote MCP URLs; configured `[mcp_servers]` entries are trusted
operator configuration.

`[responses]` configures independent response resource limits across inference, transport, and client delivery:
- `max_retained_bytes` (`AGENTIC_MAX_RETAINED_RESPONSE_BYTES`, default 8 MiB): bounds cumulative logical retained
  response data across all rounds of a turn and gateway tool outputs. Unlike raw wire bytes, it is invariant to
  upstream SSE chunking (1-byte deltas, coarse chunks, and non-streaming JSON consume identical logical budget).
- `max_upstream_json_bytes` (`AGENTIC_MAX_UPSTREAM_JSON_BYTES`, default 16 MiB): bounds an individual upstream JSON body.
- `max_upstream_sse_line_bytes` (`AGENTIC_MAX_UPSTREAM_SSE_LINE_BYTES`, default 16 MiB): bounds an individual upstream
  SSE line buffer.
- `max_stream_event_bytes` (`AGENTIC_MAX_STREAM_EVENT_BYTES`, default 16 MiB): bounds a single serialized SSE/WebSocket
  event delivered to the client, including full `response.output_item.done` snapshots and terminal `response.completed`.
  The WebSocket transport applies the same limit to each routed event (its `stream_id` member counts toward it), and
  the executor validates the terminal event against that transport limit before persisting the response.

Validation enforces that `max_stream_event_bytes`, `max_upstream_sse_line_bytes`, and `max_upstream_json_bytes` each
exceed `max_retained_bytes` by proportional wire headroom (`max(64 KiB, max_retained_bytes / 4)`) to account for JSON
serialization overhead, escaping, and message envelopes. When sizing gateway memory for concurrent streams, note that
in-flight buffers multiply per active streaming request: a stream in flight may hold an upstream line buffer (up to
`max_upstream_sse_line_bytes`), its parsed AST, an outbound channel event (up to `max_stream_event_bytes`), and retained
turn state (`max_retained_bytes`).

With that file in place, inject only the secret when starting the server:

```bash
YOU_API_KEY="<your-you.com-api-key>" agentic-server
```

## MCP servers and secrets

Restrict the file to the service account (for example, `chmod 600 ~/.agentic-api/config.toml`), especially if you add
credentialed `database_url`, MCP headers, or stdio MCP environment values. Prefer `DATABASE_URL`, referenced API-key
environment variables, and a deployment secret manager for secrets.

To use an operator-configured MCP server, declare its label without sending its connection details or secrets:

```json
{
  "type": "mcp",
  "server_label": "server_label"
}
```

Configured `allowed_tools` form the maximum tool set; request-provided `allowed_tools` may narrow it. A configured
`require_approval = "never"` lets requests omit that field. If a label exists in `config.toml`, a request cannot
override it with `server_url`; otherwise the existing request-declared HTTP MCP flow remains available.

Web search provider settings are described in [Web search providers](web-search.md), and the gateway-run web search
and web fetch tools for Claude Code in [Gateway server tools on `/v1/messages`](messages-server-tools.md).
