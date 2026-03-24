# Events Reference

This page lists all Server-Sent Events (SSE) emitted by the streaming API.

## Response Lifecycle

Events that describe the state of the overall response.

### `response.created`

Fired immediately when the connection is established.

**Payload:** `{ "response": { "id": "...", "status": "in_progress", ... } }`

### `response.in_progress`

Fired periodically or after major state changes.

**Payload:** `{ "response": { ... } }`

### `response.completed`

Fired when the response generation is successfully finished.

**Payload:** `{ "response": { "status": "completed", "output": [...], "usage": {...} } }`

### `response.incomplete`

Fired when generation reaches an incomplete terminal state (for example output-token truncation or content filtering).

**Payload:** `{ "response": { "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, ... } }`

### `response.failed`

Fired if an error occurs during generation.

**Payload:** `{ "response": { "status": "failed", "error": {...} } }`

______________________________________________________________________

## Output Items

Events related to the creation and completion of items in the `output` list (messages, tool calls).

### `response.output_item.added`

Fired when a new item (message or tool call) is initialized.

**Payload:**

```json
{
  "output_index": 0,
  "item": { "type": "message", "id": "...", "status": "in_progress" }
}
```

### `response.output_item.done`

Fired when an item is fully generated.

**Payload:**

```json
{
  "output_index": 0,
  "item": { "status": "completed", "content": [...] }
}
```

______________________________________________________________________

## Content Parts

Events for parts within an output item (e.g., text blocks within a message).

### `response.content_part.added`

Fired when a new content part begins.

**Payload:** `{ "output_index": 0, "content_index": 0, "part": { "type": "text" } }`

### `response.content_part.done`

Fired when a content part is finished.

**Payload:** `{ "output_index": 0, "content_index": 0, "part": { ... } }`

______________________________________________________________________

## Text Generation

### `response.output_text.delta`

Fired when new text tokens are generated.

**Payload:** `{ "delta": "...", "output_index": 0, "content_index": 0 }`

### `response.output_text.done`

Fired when the text block is complete.

**Payload:** `{ "text": "Full text...", ... }`

______________________________________________________________________

## Tool Calls

### Function Calling (Custom)

#### `response.function_call_arguments.delta`

Streaming JSON arguments for a function call.

**Payload:** `{ "delta": "{\"loc\":", ... }`

#### `response.function_call_arguments.done`

Final arguments string.

**Payload:** `{ "arguments": "{\"loc\":\"Paris\"}", ... }`

### Code Interpreter (Built-in)

#### `response.code_interpreter_call.in_progress`

The tool call has started.

#### `response.code_interpreter_call_code.delta`

The model is writing the Python code to be executed.

**Payload:** `{ "delta": "print(..." }`

#### `response.code_interpreter_call_code.done`

The code block is complete.

#### `response.code_interpreter_call.interpreting`

The code has been sent to the runtime and is executing.

#### `response.code_interpreter_call.completed`

Execution has finished (logs/images are available in the final item).

### MCP Tool Calls (Hosted + Client-Specified Remote)

#### `response.mcp_call.in_progress`

MCP call has started.

#### `response.mcp_call_arguments.delta`

Streaming MCP arguments JSON chunks.

**Payload:** `{ "delta": "{\"query\":", ... }`

#### `response.mcp_call_arguments.done`

Final MCP arguments JSON string.

**Payload:** `{ "arguments": "{\"query\":\"migration notes\"}", ... }`

#### `response.mcp_call.completed`

MCP call completed successfully.

**Payload:** `{ "output": "{...}", ... }`

#### `response.mcp_call.failed`

MCP call failed at item level.

**Payload:** `{ "item_id": "...", "output_index": 2, "sequence_number": 53, ... }`

When `response.output_item.done` is emitted for MCP, the item type is `mcp_call` and includes:

- `server_label`
- `name`
- `arguments`
- `status` (`completed` or `failed`)
- `output` (on success) or `error` (on failure)

______________________________________________________________________

## Reasoning

For models that support reasoning (Chain of Thought).

### `response.reasoning.delta`

Streaming reasoning content.

### `response.reasoning.done`

Reasoning is complete.
