# Streaming Events (SSE)

The Responses API uses **Server-Sent Events (SSE)** to stream the response as it is generated. This allows your application to display output incrementally, improving perceived latency.

## Overview

Unlike the Chat Completions API which streams simple text deltas, the Responses API streams **rich lifecycle events**. This means you get explicit notifications when:

- A new output item (message, tool call, thinking block) starts.
- A content part (text, image) starts.
- Data is appended (delta).
- An item or part finishes.

## Event Ordering Principles

The gateway strictly enforces the **"Scaffold Before Delta"** principle:

1. **Added**: An object is "added" first (e.g., `response.output_item.added`). This gives you the ID and metadata.
1. **Delta**: Content is streamed via delta events (e.g., `response.output_text.delta`).
1. **Done**: The object is marked as "done" (e.g., `response.output_item.done`).

This guarantees that you never receive data for an item you haven't seen initialized.

## Event Reference

Here is the sequence of events for a standard text response:

### 1. Response Lifecycle

The stream always begins with the response object itself.

```json
event: response.created
data: {"response":{"id":"resp_123","status":"in_progress",...}}

event: response.in_progress
data: {"response":{...}}
```

### 2. Output Item Added

When the model starts generating a message.

```json
event: response.output_item.added
data: {
  "output_index": 0,
  "item": {
    "id": "msg_456",
    "type": "message",
    "role": "assistant",
    "status": "in_progress"
  }
}
```

### 3. Content Part Added

A message can contain multiple parts (text, images).

```json
event: response.content_part.added
data: {
  "output_index": 0,
  "content_index": 0,
  "item_id": "msg_456",
  "part": {"type": "text", "text": ""}
}
```

### 4. Text Deltas

The actual tokens stream in here.

```json
event: response.output_text.delta
data: {
  "output_index": 0,
  "content_index": 0,
  "item_id": "msg_456",
  "delta": "Hello"
}

event: response.output_text.delta
data: { ... "delta": " world" ... }
```

### 5. Completion Events

Each level closes gracefully.

```json
event: response.output_text.done
data: { ... "text": "Hello world" }

event: response.content_part.done
data: { ... }

event: response.output_item.done
data: {
  "output_index": 0,
  "item": {
    "id": "msg_456",
    "status": "completed",
    "content": [{"type":"text","text":"Hello world"}]
  }
}

event: response.completed
data: {
  "response": {
    "id": "resp_123",
    "status": "completed",
    "output": [...]
  }
}
```

Terminal lifecycle event can be one of:

- `response.completed` for successful completion
- `response.incomplete` for token/content-filter truncation
- `response.failed` for request/stream failure paths

For stateful continuation, treat `response.completed` and `response.incomplete` as terminal response objects. If `store=true`, both can be used as `previous_response_id` in the next turn.

### MCP Tool Call Sequence

When using MCP tools (hosted or Client-Specified Remote), an output item can include additional tool-call events:

```json
event: response.output_item.added
data: {
  "output_index": 0,
  "item": {
    "id": "mcp_123",
    "type": "mcp_call",
    "status": "in_progress"
  }
}

event: response.mcp_call.in_progress
data: { ... }

event: response.mcp_call_arguments.delta
data: { "delta": "{\"query\":" ... }

event: response.mcp_call_arguments.done
data: { "arguments": "{\"query\":\"migration notes\"}" ... }

event: response.mcp_call.completed
data: { "output": "{\"results\":[...]}" ... }

event: response.output_item.done
data: {
  "output_index": 0,
  "item": {
    "id": "mcp_123",
    "type": "mcp_call",
    "status": "completed",
    "output": "{\"results\":[...]}"
  }
}
```

Failure path:

```json
event: response.mcp_call.failed
data: {
  "item_id": "mcp_123",
  "output_index": 0,
  "sequence_number": 12
}

event: response.output_item.done
data: {
  "output_index": 0,
  "item": {
    "id": "mcp_123",
    "type": "mcp_call",
    "status": "failed",
    "error": "tools/call timeout after 60s"
  }
}
```

### 6. Terminal Marker

The stream ends with a specific marker, signaling the connection can be closed.

```text
data: [DONE]

```

!!! info "Spec Compliance"

    The gateway emits `data: [DONE]\n\n` as required by the Open Responses specification.

## Parsing the Stream

If you are using the OpenAI Python SDK, this complexity is handled for you:

```python
with client.responses.stream(...) as stream:
    for event in stream:
        # The SDK parses the event and gives you a typed object
        print(event.type)
```

If you are parsing manually (e.g., in a web frontend), ensure you listen for `response.output_text.delta` to accumulate text.
