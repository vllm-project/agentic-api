# Agentic API Playground

A buildless browser application that talks directly to Agentic API over HTTP/SSE or WebSocket. Explore multi-agent tasks,
built-in tools, remote MCP, your own client functions, token usage, and conversation compaction. No coding harness
or application backend is involved. The gateway runs agents and server tools; the page handles client tools and
submits their outputs through Responses continuations.

## Run locally

From the repository root, build the gateway with Python execution support:

```bash
./scripts/setup-eryx-runtime.sh
cargo build --release --locked -p agentic-server --bin agentic-server --features embedded-code-interpreter
```

See [interpreter provisioning](../../docs/design/embedded-code-interpreter.md#runtime-provisioning) for platform,
runtime, and Linux cgroup requirements. Start your inference server, then start the gateway:

```bash
YOU_API_KEY="your-key" \
AGENTIC_API_HOME=/path/to/your/.agentic-api \
AGENTIC_CODE_INTERPRETER_ENABLED=true \
CORS_ALLOWED_ORIGINS=http://localhost:8080,http://127.0.0.1:8080 \
RUST_LOG=info,agentic_server=debug,agentic_core=debug \
  ./target/release/agentic-server \
    --llm-api-base http://127.0.0.1:5050 \
    --gateway-port 9000 > gateway.log 2>&1
```

Configure your search provider in the gateway (for example, `AGENTIC_WEB_SEARCH_PROVIDER=you` and its endpoint).
Search keys belong to the gateway. Keep one `RUST_LOG` assignment; a second assignment overrides the first.
An HTTP 400 saying `code_interpreter is disabled; rebuild ...` requires rebuilding with the feature above and
restarting the gateway with that binary. Disable Python execution in the page if you do not need it.

In a second terminal, serve the repository root so the page can load the shared logo assets:

```bash
python3 -m http.server 8080 --bind 127.0.0.1 --directory .
```

Open **http://localhost:8080/application_examples/agentic_playground/**, enter **http://localhost:9000** as the gateway URL, and select **Load models**.
Enter a bearer token if required. Gateway and MCP credentials stay in page memory; previews and downloads redact
credential fields. Reloading the page clears configuration and history. Use matching HTTP/HTTPS schemes and allow
the page's exact origin in gateway CORS configuration; `localhost` and `127.0.0.1` are different origins.

## Try the demos

- **Single agent (default):** edit the general prompt and select **Send prompt**. Multi-agent execution starts off,
  with the concurrent-subagent setting at one.
- **Two agents:** choose **Two-agent coding comparison**, keep the example prompt, and select **Send prompt**.
  The coordinator researches a popular LeetCode problem, gives two solvers the same specification and examples,
  asks them to execute solutions, then compares correctness, tests, efficiency, and clarity. Scores are the model's
  assessment based on tool evidence. Delegation and tool use depend on the model; a prompt cannot guarantee them.
- **Separate assignments:** select **Separate tasks for each agent** and edit each task card.
- **Microsoft Learn:** expand **Remote MCP servers**, add Microsoft Learn, and select **Use demo prompt**.
  The gateway discovers and executes the selected documentation tools. Disable Python and web search to focus on MCP.
- **Client function:** expand **Client functions · JavaScript**, select **Use function demo**, and optionally
  **Test handler**. The model calls `summarize_numbers`; the page executes your handler and sends the real result
  back with `previous_response_id` until the model answers. You can edit the schema and handler or add more functions.
- **Compaction:** complete a turn, add a follow-up, then select **Compact conversation**. Inspect the returned
  checkpoint and its usage, and ask a question that depends on earlier context. The next request sends the returned
  response ID. Compaction preserves the existing agent tree and stored continuation state.

## Tools and settings

| Tool | Execution in this example |
| --- | --- |
| `web_search` | Gateway-configured search provider |
| `code_interpreter` | Gateway's embedded Python sandbox; fresh state on every call |
| `mcp` | Remote HTTP MCP endpoint, allowed tool names, optional authorization and headers |
| `function` | User-authored JavaScript handler with model-provided JSON arguments |
| `custom` | User-authored handler receiving free-form text; constrained formats are not offered |
| `namespace` | Groups client function declarations and routes calls by namespace and name |
| `tool_search` | Client discovery returns the configured deferred client definitions |
| `shell` | Pauses for you to run commands elsewhere and enter their actual outputs and exit codes |

The browser does not execute shell commands or edit local repositories. `file_search` is not implemented by this
gateway and is not offered. MCP uses `require_approval: "never"`, the gateway's supported approval mode; add servers
and allowed tools you intend to execute automatically.

JavaScript handlers run in a worker inside an opaque-origin sandboxed iframe. They receive only their arguments,
without gateway credentials or page state. CSP blocks network access, and workers have no DOM or localStorage.
Execution is limited to five seconds; handler, argument, and result sizes are bounded. This is a small local demo,
not an operating-system resource sandbox: it does not enforce a memory quota. Only code you enter in the handler
editor is evaluated; model arguments are data. Errors are returned as tool outputs for the model to inspect.

Responses settings expose `parallel_tool_calls`, `max_tool_calls`, maximum output tokens, tool choice, sampling,
reasoning effort/summary, text/JSON output, additional instructions, and automatic compaction thresholds.
**Parallel tool calling and maximum tool calls are separate settings.** This gateway rejects `max_tool_calls`
and reasoning summaries with multi-agent execution, so those controls are disabled until multi-agent is off.
The subagent limit controls team concurrency; the browser-function limit controls local client execution.
The client continuation/injection limit bounds automatic follow-up requests or live WebSocket client outputs. Model support for sampling, reasoning, and
structured output varies. The API tabs select Responses chains or durable Conversations; neither is a complete API administration console.

## Responses and Conversations tabs

**Responses API** sends each prompt as an individual request. Both **Send prompt** and the follow-up field use the
latest completed `previous_response_id`; only **New session** clears the chain. Request settings can change between
turns. Storage defaults to **store:true**. Multi-agent execution always requires storage; to use **store:false**,
start a single-agent session. An existing multi-agent tree cannot be switched to single-agent execution mid-chain.

- **HTTP · SSE:** responses stream from `POST /v1/responses`. An unstored response ID cannot be continued over HTTP;
  the page explains this and does not silently discard the ID or replay a fabricated history. Client-tool loops
  that need another HTTP request also require storage.
- **WebSocket:** sends `response.create` on `WS /v1/responses`, reusing one socket and lane across prompts. This lets
  single-agent `store:false` prompts continue with their previous response ID. Losing the socket loses that
  unstored checkpoint; start a new session instead of reconnecting with an unusable ID. Multi-agent client outputs
  are returned live with `response.inject`; single-agent client calls use ordinary Responses continuations.
  Browser WebSockets cannot set an Authorization header, and this gateway does not accept a browser token via
  subprotocol or URL. Use HTTP for bearer-authenticated gateways, or deploy a trusted proxy that authenticates the
  WebSocket upgrade. The page never puts bearer tokens in URLs.

**Conversations API** creates a conversation with `POST /v1/conversations` or loads an existing ID. It lists stored
items with cursor pagination. Prompts use HTTP Responses with `conversation` and `store:true`; they omit
`previous_response_id`, since those references are mutually exclusive. The two tabs retain independent in-memory
histories and checkpoints. **New session** clears only the active tab; it does not delete server data. A conversation
is bound to the gateway that created or loaded it. Tool editors remain shared, and each prompt uses current settings.

## Remote MCP presets

| Server | Endpoint | Useful demo |
| --- | --- | --- |
| [Microsoft Learn](https://learn.microsoft.com/en-us/training/support/mcp-developer-reference) | `https://learn.microsoft.com/api/mcp` | Search documentation, fetch articles, search code samples; no authentication |
| [GitMCP](https://github.com/idosal/git-mcp) | `https://gitmcp.io/docs` | Search and fetch public repository documentation and code |
| [Hugging Face](https://huggingface.co/docs/hub/en/agents-mcp) | `https://huggingface.co/mcp` | Explore models, datasets, and documentation with `hf_fs`; check account settings and token requirements |

Before using a server, merge its hostname into `[mcp].allowed_hosts` in `$AGENTIC_API_HOME/config.toml`, preserving
existing entries. For example, to allow Microsoft Learn and GitMCP:

```toml
[mcp]
allowed_hosts = ["learn.microsoft.com", "gitmcp.io"]
```

Restart the gateway after updating configuration. The page cannot change the gateway's trust configuration.
Add `huggingface.co` only if you choose that preset. Availability and tool catalogs are controlled by the provider.
The Learn preset restricts calls to `microsoft_docs_search`, `microsoft_docs_fetch`, and `microsoft_code_sample_search`.

## Progress, tokens, and context

Agent cards project **observed event types** (SSE over HTTP, JSON frames over WebSocket) into hints such as spawned, thinking, searching, executing Python,
and using MCP. Each step shows its exact event type. Active progress bars are indeterminate because SSE does not
provide a reliable percentage. Spawn confirmation comes from successful collaboration output, not merely a request.
Client execution is separately labeled as browser activity. Thinking indicates reasoning activity, not access to
hidden chain of thought. The agent count includes the root agent. The header has no decorative agent diagram. With multi-agent execution
enabled and concurrency greater than one, a branching tree appears in Agent progress only after actual subagents
are observed. Select a tree node to filter that agent’s detailed progress.

Expand code/output/source details or the received-type inventory for more detail. Unattributed event types are
listed separately. **Debug** contains the redacted request and latest 250 raw events, rather than filling the
main activity view with JSON. Downloads include each turn's requests, responses, output items, and bounded event tail.
The Final answer tab renders Markdown headings, tables, lists, links, quotes, and fenced code, with copy-code and
Markdown/text downloads. Raw HTML is displayed as text, and only HTTP(S) links are clickable. This small DOM-based
renderer covers common model output, not every CommonMark extension.

A completed turn does not mean its code passed tests. Stream errors and incomplete responses remain visible;
**Disconnect stream** closes the browser request without claiming that gateway execution was cancelled.

Input, output, and total token counts come from API `usage`, summed over every client-tool continuation within the
user turn. **Usage by turn** retains earlier totals and separate manual compaction entries. Missing counts display
as unavailable, not zero or an estimate. Counts include gateway-reported usage; the page cannot recover usage for
requests whose streams disconnected before terminal metadata arrived.

Manual compaction sends a Responses request with `input: [{"type":"compaction_trigger"}]` and the current
`previous_response_id` (or conversation ID). Stored/multi-agent sessions send `store:true`; the resulting response
ID is used for the next prompt. This avoids the standalone `/compact` endpoint's `store:false` rehydration, which
rejects stored agent trees. The checkpoint's `encrypted_content` is plaintext in this implementation. Automatic
compaction sends `context_management: [{"type":"compaction","compact_threshold": N}]`; the gateway evaluates its
input-size estimate before inference rounds. Automatic summary usage is included in response usage. A threshold
setting alone is not proof that compaction occurred, and this page does not invent an SSE compaction event when
none is emitted. Use the manual flow for an explicit, inspectable compaction demo. See the
[compaction guide](../../docs/guides/responses-compaction.md) for semantics and limitations.

## Checks and source layout

No dependencies or build step are required to serve the application. Run offline checks with Node.js 22 or newer:

```bash
node --test application_examples/agentic_playground/*.test.mjs
node --check application_examples/agentic_playground/app.mjs
```

Optional Chromium integration checks use synthetic API responses and do not call a model:

```bash
npm install --prefix /tmp/agentic-playground-qa playwright
export PLAYWRIGHT_BROWSERS_PATH=/tmp/agentic-playground-browsers
/tmp/agentic-playground-qa/node_modules/.bin/playwright install chromium
PLAYWRIGHT_MODULE=/tmp/agentic-playground-qa/node_modules/playwright/index.mjs \
  node application_examples/agentic_playground/browser-check.mjs
```

The browser check covers isolation, timeout/abort cleanup, client-tool continuation, token aggregation, compaction
continuation, safe Markdown, tab isolation, HTTP ID chaining, WebSocket continuity/injection, observed agent trees,
and mobile overflow. Live inference, search credentials, interpreter provisioning, and remote
MCP connectivity still need verification against your gateway.

`protocol.mjs` handles SSE framing and item state; `progress.mjs` projects progress; `settings.mjs` builds declarations
and settings; `client-tools.mjs` handles continuation outputs and usage; `sandbox.mjs` isolates handlers; `transport.mjs` handles HTTP and WebSocket lifecycles; `markdown.mjs` renders answers; `app.mjs`
connects them to the UI. The page references the repository's `assets/Black-Main-Logo.svg` and
`assets/Main-Brandmark.svg` directly through relative paths; no logo copies are maintained in the example.
