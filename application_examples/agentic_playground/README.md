# Agentic API Playground

A buildless browser application that talks directly to Agentic API over HTTP/SSE or WebSocket. Explore multi-agent tasks,
built-in tools, remote MCP, your own client functions, token usage, and conversation compaction. No coding harness
is involved. The gateway runs agents and server tools; the page handles client tools and
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

## Four-agent demo

The page defaults to four agents using Python execution, web search, Microsoft Learn MCP, and file search.
The prompt asks for a sorting algorithm, Azure Blob Storage documentation, and retention/ingest requirements from
`archive-requirements.md`, with live progress and a short summary. The model chooses agent names.

Automatically loaded sample vector-store IDs belong to the gateway that supplied them. Changing the gateway URL
clears those IDs; the next send loads a sample store on the new gateway. Manually entered IDs are preserved.

The tools and four concurrent subagents are enabled automatically. The sample vector store loads when its field is
empty. Load your served model, then select **Send prompt**. Edit the prompt if needed, or select **Use example** to
restore it. After completion, continue in the chat box.

The **Workflow** dropdown also offers **General prompt**, **Two-agent coding comparison**, and
**Separate tasks for each agent**. Choose a workflow to load its prompt, or use the task cards for separate
assignments. **Use example** restores the selected workflow's example. Shell execution is not offered.

## Tools and settings

| Tool | Execution in this example |
| --- | --- |
| `web_search` | Gateway-configured search provider |
| `code_interpreter` | Gateway's embedded Python sandbox; fresh state on every call |
| `file_search` | Gateway retrieval from uploaded documents in configured vector stores |
| `mcp` | Remote HTTP MCP endpoint, allowed tool names, optional authorization and headers |
| `function` | User-authored JavaScript handler with model-provided JSON arguments |
| `custom` | User-authored handler receiving free-form text; constrained formats are not offered |
| `namespace` | Groups client function declarations and routes calls by namespace and name |
| `tool_search` | Client discovery returns the configured deferred client definitions |

The browser does not execute shell commands or edit local repositories.
MCP uses `require_approval: "never"`, the gateway's supported approval mode; add servers
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

## Conversation messages

Sent prompts, root-agent mid-turn replies, and final answers appear in chat below live progress. The chat stage
indicator uses the same root progress events as the detailed view, including Thinking and Waiting for agents.
Tool outputs and subagent messages stay in the detailed view. Sending a follow-up immediately clears
the composer and locks it for the whole turn, including client-tool waits. It unlocks after the final response or
an error/disconnection. Enter sends; Shift+Enter adds a line. New session clears the transcript, and API tabs keep
their transcripts separate. History remains in this tab and is cleared on reload.

## File search demo

Choose **Four-agent tool demo** or **Use example**. When the vector store field is empty, the playground
finds the sample store on the configured gateway and fills in its ID automatically. If the sample is missing,
it uploads [archive-requirements.md](archive-requirements.md), creates a vector store, and fills in the new ID
after ingestion succeeds. Wait for **Sample document ready**, then select **Send prompt**.

The sample store is marked with `metadata.playground_sample: "archive-requirements.md"` and reused across page
reloads. After a database reset, reloading the page and choosing a demo creates a new sample store automatically.
The lookup follows paginated store lists and ignores empty or unfinished stores. Manually entered IDs are preserved.
**Load sample document** retries setup or replaces the field with the current sample store ID. Sending a prompt
with file search enabled and an empty ID field also prepares the sample automatically.

An HTTP 404 from `/v1/files` or `/v1/vector_stores` means the running gateway lacks the Files API: rebuild it using the command in
[Run locally](#run-locally) and restart it. Launch scripts that reuse `target/release/agentic-server` do not
automatically rebuild it when the checkout changes. When using `serve-playground.sh` or
`serve-playground-https.sh`, set `KEEP_DB=1` to retain uploaded file metadata and vector stores across restarts.

To prepare the sample manually instead, run these commands from the repository root, with the gateway at `localhost:8080`:

```bash
curl --fail-with-body http://localhost:8080/v1/files \
  -F purpose=assistants \
  -F file=@application_examples/agentic_playground/archive-requirements.md

# Replace file_REPLACE with the file ID returned above.
curl --fail-with-body http://localhost:8080/v1/vector_stores \
  -H 'Content-Type: application/json' \
  -d '{"name":"playground-archive","file_ids":["file_REPLACE"],"metadata":{"playground_sample":"archive-requirements.md"}}'
```

For an authenticated gateway, add its bearer authorization header to both commands. Creating the vector store
with `file_ids` ingests the document before the gateway returns success. Paste the returned **vector store ID**
into **File search → Vector store IDs**. Existing stores can also be used; separate multiple IDs with commas or
whitespace. The four-agent prompt expects the sample document to be present in one of those stores.

**Enable file search** adds the `file_search` declaration with `vector_store_ids` and `max_num_results` (default 5,
range 1–50). **Show retrieved passages** adds `include: ["file_search_call.results"]`, following the
[Responses file search contract](https://developers.openai.com/api/docs/guides/tools-file-search).
The activity view shows file search status and queries. Expand **View code, outputs & sources** to inspect
filenames, file IDs, relevance scores, and retrieved text. File citations on answers show their source filenames.
Disabling passage display omits retrieved text from the public call output; the model can still use retrieval.
File search settings are sent again on follow-up requests.

The gateway uses keyword retrieval without an embedding endpoint; no embedding service is needed for this sample.
See the [file search guide](../../docs/api/file-search.md) for storage, embeddings, authentication, and deletion of
the sample upload/vector store when finished. The playground can load this sample or use existing stores;
it does not provide a general file-management interface.

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

Agent filters, tree buttons and evidence panels remain interactive while output streams. Large tool outputs are
rendered when you expand their evidence panel. **Disconnect stream** is beside the run status at the top.

Agent cards project **observed event types** (SSE over HTTP, JSON frames over WebSocket) into hints such as spawned,
thinking, searching the web or files, executing Python, and using MCP. Each step shows its exact event type.
Active progress bars are indeterminate because SSE does not
provide a reliable percentage. Spawn confirmation comes from successful collaboration output, not merely a request.
Client execution is separately labeled as browser activity. Thinking indicates reasoning activity, not access to
hidden chain of thought. The agent count includes observed root activity. With multi-agent execution enabled,
the tree shows `/root` from the start, then adds branches as actual subagents are observed. Select a tree node
to filter that agent’s detailed progress; the full tree remains visible, with the selected node highlighted.

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
python3 -m unittest discover -s application_examples/agentic_playground -p '*_test.py'
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
the four-agent demos, sample upload/indexing and retry, file search validation/evidence/citations/continuation,
chat clearing/locking/history, stable clicks during a busy multi-agent stream, lazy evidence updates, and mobile overflow.
Live inference, search credentials, interpreter provisioning, and remote
MCP connectivity still need verification against your gateway.

`protocol.mjs` handles SSE framing and item state; `progress.mjs` projects progress; `settings.mjs` builds declarations
and settings; `client-tools.mjs` handles continuation outputs and usage;
`sandbox.mjs` isolates handlers; `transport.mjs` handles HTTP and WebSocket lifecycles; `markdown.mjs` renders answers; `app.mjs`
connects them to the UI. The page references the repository's `assets/Black-Main-Logo.svg` and
`assets/Main-Brandmark.svg` directly through relative paths; no logo copies are maintained in the example.
