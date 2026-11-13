# Gateway server tools on `/v1/messages`

Claude Code's own tools (Bash, Edit, Read, and so on) stay client-owned: Claude Code runs them, as usual. The native
server tools below run on the gateway instead.

## Web search

Current Claude Code versions declare Anthropic's native `web_search_20250305` server tool. Agentic API translates that
declaration for the upstream model and executes the resulting search server-side against the configured search backend
(You.com, Brave Search, Tavily, Serply, Keenable, or SearXNG, see [Web search providers](web-search.md)); no MCP server or tool
alias is required:

```bash
YOU_API_KEY=<you.com-key> YOU_API_BASE_URL=<you.com-base-url> \
  cargo run -p agentic-server -- --llm-api-base http://0.0.0.0:5050
```

The gateway supports the basic `web_search_20250305` contract, including `max_uses`, `allowed_domains`,
`blocked_domains`, and the country in `user_location`. The later versions, `web_search_20260209` and
`web_search_20260318`, add dynamic filtering, which calls the search from Anthropic's code execution by default. The
gateway does not run code execution, so it runs a later version as the basic search when its `allowed_callers` includes
`"direct"`:

```json
{"type": "web_search_20260318", "name": "web_search", "allowed_callers": ["direct"]}
```

Without `"direct"`, the request is rejected with HTTP 400 naming `allowed_callers`, as Anthropic's API answers for
models that cannot call tools from code. `response_inclusion` (`web_search_20260318`) is accepted because it only
affects results that code execution consumed. A `web_search` declaration of another version is rejected with HTTP 400
rather than forwarded in a shape the upstream cannot execute, and so is one that sets `use_cache`, which no web search
version defines, or `response_inclusion` on a version before `web_search_20260318`.

`max_uses` limits the searches performed across the whole request, not the number of tool calls. The model may batch
several queries into one call, and every query counts as one search. A call that the remaining budget cannot cover is
not run, and the model is told the limit was reached; a later call that fits still runs.

Each search the gateway runs is returned the way Anthropic returns it: a `server_tool_use` block with the model's
query, then a `web_search_tool_result` listing each result's URL, title, and page age, and
`usage.server_tool_use.web_search_requests` counts the searches that ran. Claude Code builds its result list and search
count from these blocks. The page text travels in each result's `encrypted_content`, which the gateway seals; send the
blocks back unchanged, and the next turn gives the model the same results it read before without searching again.
Altered `encrypted_content` is refused with HTTP 400, as Anthropic refuses it. A failed search shows its documented
error code. The `server_tool_use` input is the call as the model made it, in the gateway's search schema (`query`, or
`queries` for a batch).
Results stay readable across restarts and on every replica that shares `AGENTIC_MESSAGES_RESULT_KEY`, a secret of at
least 32 random bytes; without it, each process generates its own key, and results sealed by another process reach
the model as a note that they are no longer available.

Older clients that declare a function tool named `WebSearch` can still opt in with
`MESSAGES_GATEWAY_TOOL_ALIASES="WebSearch=web_search"`. This variable maps a client tool name to a gateway executor
(`name=executor`, comma-separated) and remains empty by default. The gateway adapts the older `WebSearch` function's
`allowed_domains`/`blocked_domains` arguments to the executor's schema automatically. A call through an alias is the
client's own function, so it stays hidden from the client, as does a plain function named `web_search`.

> Note: allow and block domain lists are mutually exclusive, matching Anthropic's native tool contract.

## Web fetch

Applications that call the Messages API through the Anthropic SDK can declare Claude's native
[`web_fetch`](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-fetch-tool) server tool, in its basic
version `web_fetch_20250910` or a later one. Agentic API rewrites that declaration into an ordinary function tool for
the upstream model, fetches the page itself when the model calls it, hands the page text back to the model, and
returns the call to the client as a `server_tool_use` block and a `web_fetch_tool_result` with the page text, as
Anthropic does, counted in `usage.server_tool_use.web_fetch_requests`. `/v1/messages/count_tokens` accepts the
declaration too. No search provider or API key is involved; the built-in fetcher is on by default and can be
switched off with `[web_fetch] enabled = false` or `AGENTIC_WEB_FETCH_ENABLED=false`, after which such a declaration is
rejected with HTTP 400 instead of being forwarded in a shape the upstream cannot execute.

```json
{
  "tools": [
    {"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 5,
     "allowed_domains": ["example.com"], "max_content_tokens": 20000}
  ]
}
```

The gateway supports `max_uses`, `allowed_domains` / `blocked_domains` (matched on the host only, as Anthropic documents
for web fetch, so each entry must be a host name or address without scheme or path), and `max_content_tokens`;
`citations: {"enabled": false}` and `allowed_callers: ["direct"]` are accepted. The later versions,
`web_fetch_20260209`, `web_fetch_20260309`, and `web_fetch_20260318`, add dynamic filtering and follow the web search
rule: they run as the basic fetch when `allowed_callers` includes `"direct"` and are rejected with HTTP 400 otherwise.
`use_cache` (`web_fetch_20260309` and later) is honoured whatever its value, because the built-in fetcher keeps no cache
of fetched pages, and `response_inclusion` (`web_fetch_20260318`) is accepted because it only affects results that code
execution consumed. Anything the gateway cannot honour (`citations` enabled, `use_cache` or `response_inclusion` on a
version that does not define it, or another `web_fetch_*` version) is rejected with HTTP 400 rather than ignored.

Each call answers the model with one JSON `tool_result`: the final URL after redirects, the page title, the content
type, a `retrieved_at` timestamp, and the page text. HTML is reduced to plain text; `text/*`, XHTML, XML, and JSON
bodies are returned as text; PDF and other binary types are refused. `max_content_tokens` is applied as an approximate
budget of four bytes per token, and the text is always cut below the gateway's 1 MiB tool output cap. A fetch that
cannot be served answers the model with the documented
[error codes](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-fetch-tool#errors) in an error
`tool_result` (`url_not_accessible`, `url_not_allowed`, `too_many_requests`, `unsupported_content_type`, `url_too_long`,
`invalid_tool_input`, `max_uses_exceeded`, `url_not_in_prior_context`, `unavailable`), so the turn continues. The
`web_fetch_tool_result` carries what the model read: beyond Anthropic's fields, the content type and truncation flag
of a page, and the message of a refusal. Sent back in a later turn, it gives the model the same text again.

Fetches are bounded and never reach internal networks:

- Only a URL that already appeared in the conversation can be fetched: in a user message, a client `tool_result`, or a
  gateway web search or fetch result, and only as that whole URL, not as the prefix of a longer one. A URL the model
  produced itself, or one that appears only in the system prompt, is refused with `url_not_in_prior_context`, matching
  Anthropic's rule.
- Only absolute `http`/`https` URLs of at most 250 characters without embedded credentials are accepted.
- Hosts that are, or resolve to, loopback, private, link-local (including cloud metadata), carrier-grade NAT, multicast,
  or other non-public addresses are refused before any connection; the connection is pinned to the addresses that
  passed the check, and every redirect hop is checked again with the same rules and domain filters. Under this policy
  the fetcher connects directly and ignores the `HTTP_PROXY` / `HTTPS_PROXY` environment, so the check and the pin
  apply to the real destination. Deployments that fetch intranet pages on purpose can set
  `[web_fetch] allow_private_networks = true`, which also restores proxy use.
- Each fetch is bounded in time (`timeout_secs`, default 20 s including redirects), size (`max_response_bytes`,
  default 10 MiB; a longer body is cut and reported as truncated), and redirects (5 hops).
- `max_uses` is a request-wide budget of fetches. Every admitted call is charged whether the page arrives, the fetch
  fails, or the URL is refused; a call whose arguments carry no URL is answered as invalid input without being charged.
- At most `max_concurrent_gateway_calls` fetches (default 5) are in flight at once across the gateway; a round that
  asks for more waits for a slot.

| Setting | Environment variable | `config.toml` key | Default |
| :--- | :--- | :--- | :--- |
| Enabled | `AGENTIC_WEB_FETCH_ENABLED` | `[web_fetch] enabled` | `true` |
| Private networks | `AGENTIC_WEB_FETCH_ALLOW_PRIVATE_NETWORKS` | `[web_fetch] allow_private_networks` | `false` |
| Download ceiling | `AGENTIC_WEB_FETCH_MAX_RESPONSE_BYTES` | `[web_fetch] max_response_bytes` | `10485760` |
| Time ceiling | `AGENTIC_WEB_FETCH_TIMEOUT_SECS` | `[web_fetch] timeout_secs` | `20` |

Claude Code is unaffected: its `WebFetch` is a client tool with its own schema that Claude Code runs locally, and a
client function named `web_fetch` without the native `type` stays client-owned. PDF text is a follow-up.
