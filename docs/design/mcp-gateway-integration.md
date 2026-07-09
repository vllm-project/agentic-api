# MCP Gateway Integration

Target: `crates/agentic-server-core/`
Reference: [mcp handlers](https://github.com/openai/codex/tree/main/codex-rs/core/src/tools/handlers), [rmcp-client](https://github.com/openai/codex/tree/main/codex-rs/rmcp-client), [turn.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/turn.rs), [rust-sdk README](https://github.com/modelcontextprotocol/rust-sdk#readme)

---

## Goal

This design uses [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) as the protocol layer for connecting to remote MCP servers. The MCP spec in rmcp gives us two patterns for implementing built-in tools inside agentic-api. Each pattern becomes a `GatewayExecutor` registered in `ToolRegistry` under a fixed tool name:

1. **MCP Resources** ([rust-sdk Resources](https://github.com/modelcontextprotocol/rust-sdk#resources)): for tools that read or fetch content by URI. `read_mcp_resource` is the first example of this pattern.
2. **MCP Tools** ([rust-sdk `#[tool]` macro](https://github.com/modelcontextprotocol/rust-sdk#tools)): for standalone MCP server tools that expose computation or actions. The `#[tool]` macro auto-generates the JSON schema, validation, and MCP wire format for a server that agentic-api connects to via `McpClientPool`. Examples:
   - **stdio MCP server** (`connect_stdio`): a locally spawned process (command + args) the gateway connects to over stdin/stdout. Users wire up their own MCP server process and the gateway talks to it via rmcp's `TokioChildProcess` transport.
   - **calculator** (reference example from [rust-sdk Tools](https://github.com/modelcontextprotocol/rust-sdk#tools)): simplest possible `#[tool]` server, useful as a template.

### First built-in tool: `read_mcp_resource`

The first built-in is `read_mcp_resource`, mirroring codex's [`ReadMcpResourceHandler`](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/mcp_resource/read_mcp_resource.rs). Given a `server` label and a `uri`, it calls `resources/read` on the named MCP server and returns the content to the model. It is registered once in `ToolRegistry`; the model selects the target server at call time via the `server` argument. This mirrors how codex registers `read_mcp_resource` as a fixed tool regardless of how many MCP servers are connected.

### Second built-in tool roadmap: stdio MCP server support

The next built-in adds stdio transport to `McpClient`, enabling gateway-managed MCP servers that run as local processes. This uses rmcp's `TokioChildProcess` stdio transport and follows the MCP Tools pattern.

`McpClient` gains a second constructor:

```rust
impl McpClient {
    // existing: HTTP/SSE transport
    pub async fn connect(server_url: &str, headers: Option<HashMap<String, String>>) -> Result<Self, McpError>

    // new: spawns command and connects over stdin/stdout
    pub async fn connect_stdio(command: &str, args: &[&str]) -> Result<Self, McpError>
}
```

`McpClientPool` gains a config-based constructor for gateway-managed servers:

```rust
pub enum McpServerEntry {
    Http  { url: String, headers: Option<HashMap<String, String>> },
    Stdio { command: String, args: Vec<String> },
}

impl McpClientPool {
    // existing: built from client request
    pub async fn from_params(params: &[McpToolParam]) -> Self

    // new: built from gateway config at startup
    pub async fn from_config(servers: HashMap<String, McpServerEntry>) -> Self
}
```

This enables users to wire up fixture servers the same way as `.codex/config.toml`:

```toml
[mcp_servers.my_fixture]
command = "python3"
args = ["/path/to/server.py"]
```

The rest of the stack (`McpHandler`, `build_mcp_registry`, dispatch loop) is unchanged. Stdio servers are just another entry in the pool.

---

## Implementing MCP and the First Built-in Tool

### Step 1: `McpClient` (`mcp/client.rs`)

Thin async wrapper around `rmcp::service::RunningService<RoleClient>`. Connects over HTTP/SSE (streamable HTTP transport).

```rust
pub struct McpClient {
    inner: Arc<rmcp::service::RunningService<RoleClient>>,
    tool_timeout: Duration,
}

impl McpClient {
    pub async fn connect(server_url: &str, headers: Option<HashMap<String, String>>) -> Result<Self, McpError>
    pub async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>, McpError>
    pub async fn call_tool(&self, name: &str, arguments: Option<Value>) -> Result<rmcp::model::CallToolResult, McpError>
    pub async fn read_resource(&self, uri: &str) -> Result<rmcp::model::ReadResourceResult, McpError>
}
```

The `read_resource` method maps directly to the `read_resource` handler shown in the [rust-sdk Resources section](https://github.com/modelcontextprotocol/rust-sdk#resources). In the SDK, a server implements `ServerHandler::read_resource()` to serve content by URI - `McpClient::read_resource()` is the client side of that same call.

---

### Step 2: `McpClientPool` (`mcp/mod.rs`)

One `McpClient` per server, keyed by `server_label`. Has two constructors: one built from the client request (`from_params`), one built from gateway config at startup (`from_config`, for gateway-managed stdio/HTTP servers). The `ReadResource` handler holds an `Arc<McpClientPool>` and uses `get()` at call time to route to the server named in the model's arguments.

```rust
pub struct McpClientPool {
    clients: HashMap<String, Arc<McpClient>>,
}

impl McpClientPool {
    pub async fn from_params(params: &[McpToolParam]) -> Self
    pub async fn from_config(servers: HashMap<String, McpServerEntry>) -> Self
    pub fn get(&self, server_label: &str) -> Option<&Arc<McpClient>>
}
```

**Codex parallel:** [`McpConnectionManager`](https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/connection_manager.rs).

---

### Step 3: `McpHandler` and `McpHandlerKind` (`mcp/handlers/mod.rs`)

A single generic handler covers all MCP operations. `McpHandlerKind` tells `execute()` which wire operation to perform - no separate handler type per operation.

```rust
pub enum McpHandlerKind {
    /// tools/call - one handler per discovered tool, bound to a specific server
    ToolCall { tool_name: String },

    /// resources/read - registered once as the read_mcp_resource built-in.
    /// Holds the full pool so it can route to whichever server the model names
    /// in the `server` argument at call time.
    ReadResource { pool: Arc<McpClientPool> },
}

pub struct McpHandler {
    server_label: String,   // used by ToolCall; ignored by ReadResource
    client: Arc<McpClient>, // used by ToolCall; ReadResource looks up client from pool
    kind: McpHandlerKind,
}

impl GatewayExecutor for McpHandler {
    fn execute(&self, _name, arguments, _config) -> Pin<Box<...>> {
        Box::pin(async move {
            match &self.kind {
                McpHandlerKind::ToolCall { tool_name } => {
                    let result = self.client.call_tool(tool_name, serde_json::from_str(arguments).ok()).await?;
                    Ok(ToolOutput { call_id: String::new(), output: call_result_to_text(&result) })
                }
                McpHandlerKind::ReadResource { pool } => {
                    let args: ReadResourceArgs = serde_json::from_str(arguments)?;
                    let client = pool.get(&args.server)
                        .ok_or_else(|| ToolError::Execution(format!("unknown server: {}", args.server)))?;
                    let result = client.read_resource(&args.uri).await?;
                    Ok(ToolOutput { call_id: String::new(), output: read_result_to_text(&result) })
                }
            }
        })
    }
}
```

`read_resource.rs` holds only the args struct and the tool spec - not a handler itself:

```rust
#[derive(Deserialize)]
pub struct ReadResourceArgs { pub server: String, pub uri: String }

pub fn read_mcp_resource_spec() -> FunctionTool { ... }
```

Tool spec injected into every LLM request when MCP servers are present:

```json
{
  "name": "read_mcp_resource",
  "description": "Read a resource by URI from a connected MCP server.",
  "parameters": {
    "properties": {
      "server": {"type": "string", "description": "The server_label to read from."},
      "uri":    {"type": "string", "description": "The resource URI to read."}
    },
    "required": ["server", "uri"]
  }
}
```

**Codex parallel:** [`McpHandler`](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/mcp.rs) for `ToolCall`, [`ReadMcpResourceHandler`](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/mcp_resource/read_mcp_resource.rs) for `ReadResource`.

---

### Step 4: `ToolEntry` + `ToolRegistry::dispatch()` (`tool/registry.rs`)

Add `handler` to `ToolEntry`. Client-owned tools have `handler: None` and pass through to the model. Gateway-owned tools have `handler: Some(...)` and are executed by the gateway.

```rust
pub struct ToolEntry {
    pub tool_type: ToolType,
    pub config: Value,
    pub server_label: Option<String>,
    pub handler: Option<Arc<dyn GatewayExecutor>>,   // new
}

impl ToolRegistry {
    pub async fn dispatch(&self, call: &FunctionToolCall) -> Option<Result<ToolOutput, ToolError>> {
        let entry = self.entries.get(&call.name)?;
        let handler = entry.handler.as_ref()?;
        let mut out = handler.execute(&call.name, &call.arguments, &entry.config).await;
        if let Ok(ref mut o) = out { o.call_id = call.call_id.clone(); }
        Some(out)
    }
}
```

---

### Step 5: `build_mcp_registry()` (`mcp/mod.rs`)

Calls `tools/list` on each server, registers one `McpHandler` per discovered tool, and registers the `read_mcp_resource` built-in. The dispatch loop calls `registry.dispatch(call)` without knowing which kind fires.

```rust
pub async fn build_mcp_registry(pool: Arc<McpClientPool>) -> (Vec<FunctionTool>, ToolRegistry) {
    let mut specs = vec![read_mcp_resource_spec()];
    let mut entries = Vec::new();

    // Built-in: registered once; routes to the right server at call time via args.server.
    // server_label and client are unused for ReadResource - routing goes through the pool.
    entries.push(("read_mcp_resource".into(), ToolEntry {
        tool_type: ToolType::Mcp,
        config: Value::Null,
        server_label: None,
        handler: Some(Arc::new(McpHandler {
            server_label: String::new(),
            client: Arc::new(McpClient::placeholder()),
            kind: McpHandlerKind::ReadResource { pool: Arc::clone(&pool) },
        })),
    }));

    // Per-tool: one entry per discovered tool per server, keyed "{server_label}__{tool_name}"
    for (server_label, client) in pool.iter() {
        for tool in client.list_tools().await.unwrap_or_default() {
            let key = format!("{server_label}__{}", tool.name);
            specs.push(mcp_tool_to_function_tool(&key, &tool));
            entries.push((key, ToolEntry {
                handler: Some(Arc::new(McpHandler {
                    kind: McpHandlerKind::ToolCall { tool_name: tool.name.to_string() }, ..
                })),
                ..
            }));
        }
    }

    (specs, ToolRegistry::with_entries(entries))
}
```

---

## Plugging Into the Execution Loop

### How Codex Orchestrates Turns

Codex processes turns in an event-driven loop in [`try_run_sampling_request()`](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/turn.rs#L1072). Tool calls are spawned as their arguments finish arriving and run concurrently with the rest of the SSE stream. [`drain_in_flight()`](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/turn.rs#L1853) collects all results once `ResponseCompleted` arrives, then the outer `run_turn()` loop re-enters with results appended to the conversation.

```
try_run_sampling_request()
  |- OutputItemDone(FunctionCall)  -> dispatch_tool_call()
  |                                   push future -> in_flight: FuturesOrdered
  |- ResponseCompleted             -> drain_in_flight()
  |                                   sess.record_conversation_items(results)
  +- outer run_turn() loop         -> next LLM call with updated history
```

### How agentic-api Does It

The existing `ResponseAccumulator` uses a `spawn_blocking` worker for SSE parsing and an `IndexMap` to track in-flight items in insertion order. After `ResponseCompleted`, `finalize_all()` drains the `IndexMap` into `output` preserving insertion order.

`from_stream_with_dispatch` adds an optional `ToolDispatchFn`. After the stream ends, it iterates the completed `FunctionCall` items from `output` in the async context (outside the blocking worker) and executes each one sequentially. Insertion order is preserved via `IndexMap`.

```
ResponseAccumulator::from_stream_with_dispatch()
  |- spawn_blocking worker: SSE parsing, finalize_all() -> output: Vec<OutputItem>
  +- async context: for each FunctionCall in output (IndexMap insertion order):
       dispatch_fn(call).await -> ToolCallResult   (sequential)
     return (acc, Vec<ToolCallResult>)
```

**Codex parallel:**
- `from_stream_with_dispatch` = [`try_run_sampling_request`](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/turn.rs#L1072)
- sequential drain = [`drain_in_flight()`](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/turn.rs#L1853) (codex uses `FuturesOrdered` for concurrent dispatch)

**Future upgrade path:** once the `spawn_blocking` worker is replaced with a fully async SSE parser, tool dispatch can be upgraded to `FuturesOrdered` to match codex - dispatching calls as their arguments arrive rather than after the full stream completes.

---

### Step 6: Extend `ResponseAccumulator` (`executor/accumulator.rs`)

`ToolDispatchFn` is the bridge between the accumulator and the registry. When a `FunctionCall` event arrives, the accumulator reads the tool name from the event to identify which tool needs to be called (dispatch). It does not call the tool itself; it fires the closure, which looks up the handler in `ToolRegistry` by name and executes it. The accumulator only sees a `ToolCallResult` come back.

```
FunctionCallArgumentsDone
  -> read call.name                      <- identify which tool was requested
  -> ToolDispatchFn(call)                <- dispatch: find handler in registry
       -> ToolRegistry::dispatch(call)   <- lookup by name, None for client-owned tools
       -> handler.execute(arguments)     <- execute: McpClient::call_tool / read_resource
  -> ToolCallResult                      <- accumulator receives result, knows nothing else
```

```rust
pub type ToolDispatchFn = Arc<dyn Fn(FunctionToolCall) -> BoxFuture<'static, ToolCallResult> + Send + Sync>;
```

No new fields on `ResponseAccumulator` , the existing `IndexMap` already collects `FunctionCall` items in insertion order. The dispatch happens in the async context after the blocking worker finishes, not during streaming.

New constructor that runs tool dispatch sequentially after the stream ends:

```rust
pub async fn from_stream_with_dispatch(
    stream: ...,
    conversation_id: Option<&str>,
    dispatch: Option<ToolDispatchFn>,
) -> ExecutorResult<(Self, Vec<ToolCallResult>)>
```

---

### Step 7: `execute_with_mcp()` (`executor/engine.rs`)

New function alongside `execute()`. The existing `execute()` is not modified - this keeps MCP work and other tool work independent until both are stable, at which point they are unified into a single `execute_loop()`.

```rust
pub async fn execute_with_mcp(request: RequestPayload, exec_ctx: Arc<ExecutionContext>) -> ExecutorResult<ResponsePayload> {
    let pool = Arc::new(McpClientPool::from_params(&collect_mcp_params(&request)).await);
    let (extra_specs, registry) = build_mcp_registry(Arc::clone(&pool)).await;
    let registry = Arc::new(registry);

    let mut ctx = rehydrate_conversation(request, &exec_ctx).await?;
    ctx.enriched_request.append_tools(extra_specs);

    let dispatch: ToolDispatchFn = Arc::new({
        let registry = Arc::clone(&registry);
        move |call| Box::pin(async move {
            let output = registry.dispatch(&call).await
                .unwrap_or(Err(ToolError::Execution(format!("no handler: {}", call.name))));
            ToolCallResult { call, output }
        })
    });

    for _ in 0..MAX_TOOL_ROUNDS {
        let stream = call_inference(&ctx, ...);
        let (acc, tool_results) = ResponseAccumulator::from_stream_with_dispatch(
            stream, ctx.conversation_id.as_deref(), Some(Arc::clone(&dispatch)),
        ).await?;

        let mut payload = acc.finalize(...);
        ctx.inject_ids(&mut payload);

        if tool_results.is_empty() {
            persist_if_needed(payload.clone(), &ctx, &exec_ctx).await;
            return Ok(payload);
        }
        ctx = ctx.append_tool_results(payload.output, tool_results);
    }
    Err(ExecutorError::ToolLoopExceeded(MAX_TOOL_ROUNDS))
}
```

Tool errors become error text in `FunctionCallOutput`, never fatal - mirrors [`failure_response()`](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/parallel.rs#L64) in codex.

---

## Full Turn Flow

```
execute_with_mcp()
  |- McpClientPool::from_params()    connect to MCP servers
  |- build_mcp_registry()            tools/list each server; register read_mcp_resource
  |- rehydrate_conversation()        load history
  |- append_tools(extra_specs)       inject read_mcp_resource spec into request
  +- LOOP
       |- call_inference()           SSE stream from LLM
       +- from_stream_with_dispatch()
            |- spawn_blocking worker: SSE parsing, IndexMap tracks FunctionCall items
            +- ResponseCompleted -> finalize_all() -> output: Vec<OutputItem>
                 async context: for each FunctionCall in output (insertion order):
                   dispatch_fn(call).await
                     +- registry.dispatch(call)
                          +- McpHandler::execute()
                               |- ToolCall    -> McpClient::call_tool()
                               +- ReadResource -> McpClient::read_resource()
                 -> tool_results: Vec<ToolCallResult>
                 |- tool_results empty -> return payload
                 +- else              -> append_tool_results() -> next LLM call
```

---

## Implementation Order and PR Split

**PR 1: MCP client + registry infrastructure** (phases 1-5, no engine changes)

| Phase | Work | Files |
|-------|------|-------|
| 1 | `McpClient` (connect, connect_stdio, list_tools, call_tool, read_resource) | `mcp/client.rs`, `Cargo.toml` |
| 2 | `McpClientPool::from_params()`, `from_config()`, `McpServerEntry` | `mcp/mod.rs` |
| 3 | `ReadResourceArgs` + `read_mcp_resource_spec()` | `mcp/handlers/read_resource.rs` |
| 4 | `handler` on `ToolEntry`; `ToolRegistry::dispatch()` | `tool/registry.rs` |
| 5 | `McpHandler`, `McpHandlerKind`, `build_mcp_registry()` | `mcp/handlers/mod.rs`, `mcp/mod.rs` |

Neither `engine.rs` nor `accumulator.rs` is touched. Fully testable against a mock MCP server.

**PR 2: Execution loop with MCP** (phases 6-8, depends on PR 1)

| Phase | Work | Files |
|-------|------|-------|
| 6 | `ToolDispatchFn`, `in_flight_tasks`, `from_stream_with_dispatch()` | `executor/accumulator.rs` |
| 7 | `RequestContext::append_tool_results()` | `executor/request.rs` |
| 8 | `execute_with_mcp()` | `executor/engine.rs` |

PR 2 imports `McpClientPool`, `build_mcp_registry()`, and `ToolRegistry::dispatch()` from PR 1 and cannot merge first.

**Future:** once both PRs are stable, `execute()` and `execute_with_mcp()` are unified into `execute_loop()`.
