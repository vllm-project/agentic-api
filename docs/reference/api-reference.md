# API Reference

The gateway exposes a primary OpenAI Responses endpoint plus compatibility passthrough endpoints.

!!! tip "Spec Conformance"

    The gateway implements the [OpenResponses](https://www.openresponses.org) specification. This ensures schema validity and correct event ordering.

______________________________________________________________________

## OpenAI Responses API Compatibility

The gateway implements the OpenAI Responses API specification as defined in the [OpenResponses specification](https://www.openresponses.org/specification). This includes:

- **Response structure**: Full `ResponseResource` shape with all required fields
- **Streaming events**: Complete SSE event lifecycle with correct ordering
- **Output items**: Support for messages, function calls, reasoning, and built-in tools
- **Statefulness**: `previous_response_id` for multi-turn conversations

For details on the underlying architecture, see [Architecture](../getting-started/architecture.md).

### Gateway Extensions

In addition to OpenResponses contract compatibility, this gateway provides MCP integration:

- Request-time MCP declarations in Built-in MCP mode (`server_label`) and Remote MCP mode (`server_url`)
- Built-in MCP discovery endpoints (`/v1/mcp/servers`, `/v1/mcp/servers/{server_label}/tools`)
- MCP lifecycle stream events (`response.mcp_call.*`) for both Built-in MCP and Remote MCP calls

#### MCP Compatibility Matrix (Current)

| Capability                                                  | OpenAI Responses API                                            | This Gateway (Current)                                                                |
| ----------------------------------------------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| Request-declared remote MCP (`tools[].mcp.server_url`)      | Supported                                                       | Supported as Remote MCP (URL checks enabled by default; no connector mode)            |
| Request-declared connector MCP (`tools[].mcp.connector_id`) | Supported                                                       | Not supported                                                                         |
| `server_label` resolution                                   | Request-local label associated with request-declared MCP target | Built-in MCP mode: gateway-configured label. Remote MCP mode: request-local label.    |
| Where MCP transport is defined                              | Request payload                                                 | Built-in MCP: gateway config; Remote MCP: request `server_url`                        |
| Local `stdio` MCP servers                                   | Not available via request-declared MCP in OpenAI cloud runtime  | Supported in Built-in MCP config (`mcpServers.<label>.command`); not request-declared |
| `require_approval` options                                  | Richer approval forms are documented                            | `never` only                                                                          |

This table reflects current behavior in this repository as of 2026-02-28.

## Create Response

`POST /v1/responses`

Creates a model response for the given chat conversation.

### Request Body

The request body should be a JSON object with the following parameters:

| Parameter                | Type                 | Required | Description                                                                                                                        |
| ------------------------ | -------------------- | -------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| **model**                | `string`             | Yes      | The ID of the model to use (e.g., `meta-llama/Llama-3.2-3B-Instruct`).                                                             |
| **input**                | `string` or `array`  | Yes      | The input to the model. Can be a simple string prompt or a list of message objects.                                                |
| **stream**               | `boolean`            | No       | If `true`, the response is streamed as [Server-Sent Events](../usage/streaming-events.md). Default: `false`.                       |
| **previous_response_id** | `string`             | No       | The ID of a previous response. Used to continue a conversation without re-sending history.                                         |
| **tools**                | `array`              | No       | A list of tools the model can call. Supports custom `function`, built-in tools, and MCP declarations (Built-in MCP or Remote MCP). |
| **tool_choice**          | `string` or `object` | No       | Controls which tool is called. Options: `auto`, `none`, `required`, function selection, or MCP selection. Default: `auto`.         |
| **store**                | `boolean`            | No       | Whether to store the response in the database. Default: `true`. Stored responses can be reused via `previous_response_id`.         |
| **include**              | `array`              | No       | List of additional fields to include in the output (e.g., `code_interpreter_call.outputs`).                                        |
| **temperature**          | `float`              | No       | Sampling temperature between 0 and 2. Default: `1.0`.                                                                              |
| **top_p**                | `float`              | No       | Nucleus sampling probability mass. Default: `1.0`.                                                                                 |
| **max_output_tokens**    | `integer`            | No       | Maximum number of tokens to generate.                                                                                              |
| **max_tool_calls**       | `integer`            | No       | Pass-through parity field. Returned in response metadata but not enforced as a hard runtime cap by the gateway.                    |
| **instructions**         | `string`             | No       | A system/developer message to guide the model's behavior. Not persisted across `previous_response_id`.                             |
| **reasoning**            | `object`             | No       | Configuration for reasoning models (e.g. `effort`).                                                                                |

#### Input Item Schema

When `input` is an array, each item can be a **Message** or a **Tool Output**.

**Message (User/System):**

```json
{
  "role": "user",
  "content": "Hello world"
}
```

**Function Tool Output:**

```json
{
  "type": "function_call_output",
  "call_id": "call_123",
  "output": "Result string"
}
```

#### MCP Tools

MCP tools are declared in `tools` using `type: "mcp"`.

- Built-in MCP mode: provide `server_label` only.
- Remote MCP mode: provide `server_label` and `server_url`.

**MCP tool declaration:**

```json
{
  "type": "mcp",
  "server_label": "github_docs",
  "allowed_tools": ["search_docs"],
  "require_approval": "never"
}
```

**Remote MCP declaration:**

```json
{
  "type": "mcp",
  "server_label": "docs_remote",
  "server_url": "https://mcp.example.com/sse",
  "authorization": "$DOCS_ACCESS_TOKEN",
  "headers": {"X-Docs-Tenant": "acme"},
  "allowed_tools": ["search_docs"],
  "require_approval": "never"
}
```

**Force MCP tool choice:**

```json
{
  "type": "mcp",
  "server_label": "github_docs",
  "name": "search_docs"
}
```

MCP request validation rules include:

- `tool_choice.type="mcp"` requires at least one matching MCP declaration in `tools`.
- Duplicate MCP declarations for the same `server_label` are rejected (including Built-in MCP / Remote MCP cross-mode duplicates).
- Hosted `server_label` values must reference available configured servers.
- Remote MCP URL checks are enabled by default and enforce `https` plus denylist host policy (`localhost`, `*.localhost`, IP-literal hosts).
- Remote MCP URL checks can be disabled by setting `VR_MCP_REQUEST_REMOTE_URL_CHECKS=false`.
- Remote MCP execution can be disabled by `VR_MCP_REQUEST_REMOTE_ENABLED=false`.
- Remote MCP transport selection is delegated to FastMCP from request `server_url` and request headers.
- The gateway does not perform its own transport inference metadata/fallback logic for Remote MCP declarations.
- Function tool names starting with `mcp__` are reserved and rejected.
- `connector_id` is unsupported and rejected.
- Remote MCP `headers` are supported. Built-in MCP declarations reject `headers`.
- `authorization` is supported only in Remote MCP mode; it is request-scoped and not persisted.
- `require_approval` currently supports `never` only (no interactive approval flow).

MCP failure semantics:

- Remote MCP pre-run inventory/discovery failures are request-level `422` (`bad_input`).
- Runtime MCP tool failures are item-level (`mcp_call.status="failed"` with `mcp_call.error`) and do not make the response request fail by themselves.

______________________________________________________________________

### Response Body (Non-Streaming)

On success, returns a JSON object representing the response.

```json
{
  "id": "resp_01JM...",
  "object": "response",
  "created_at": 1700000000,
  "model": "meta-llama/Llama-3.2-3B-Instruct",
  "status": "completed",
  "output": [
    {
      "id": "msg_01JM...",
      "type": "message",
      "role": "assistant",
      "content": [
        {
          "type": "text",
          "text": "Hello! How can I help you?"
        }
      ]
    }
  ],
  "usage": {
    "input_tokens": 10,
    "output_tokens": 8,
    "total_tokens": 18
  }
}
```

### Response Body (Streaming)

See [Streaming Events](../usage/streaming-events.md) for the full event reference.

When generation stops due to output token limits or content filtering, Responses-style terminal semantics apply:

- `status: "incomplete"`
- `incomplete_details.reason` (for example `max_output_tokens` or `content_filter`)

If `store=true` (default), both terminal statuses `completed` and `incomplete` are persisted and can be referenced by `previous_response_id`.

If `store=false`, the response is not persisted. It cannot be retrieved later and cannot be used as a
`previous_response_id`.

______________________________________________________________________

## Retrieve Response

`GET /v1/responses/{response_id}`

Retrieves a stored Responses object by ID.

Behavior:

- If the response exists in the ResponseStore, the gateway returns the stored `response` object.
- If the response does not exist, the gateway returns `404 invalid_request_error`.
- Responses created with `store=false` are not persisted, so retrieval returns not found.

______________________________________________________________________

## Passthrough Compatibility Endpoints

The gateway also provides minimal passthrough routes for legacy OpenAI-compatible clients:

- `GET /v1/models`
- `POST /v1/chat/completions`

Passthrough behavior:

- Request and response payloads are forwarded as-is (no schema translation).
- Streaming SSE payloads are forwarded without rewriting.
- Upstream HTTP error payloads are returned unchanged.
- Transport failures map to gateway errors:
    - `502` with `error.code="upstream_unavailable"`
    - `504` with `error.code="upstream_timeout"`

These endpoints are compatibility routes only; stateful Responses features such as
`previous_response_id` remain specific to `POST /v1/responses`.

______________________________________________________________________

## MCP Discovery Endpoints

In addition to `POST /v1/responses`, the gateway exposes Built-in MCP discovery endpoints:

- `GET /v1/mcp/servers`
    - Returns configured server inventory with availability.
    - Returns an empty list when Built-in MCP is disabled.
- `GET /v1/mcp/servers/{server_label}/tools`
    - Returns runtime tool inventory for that server.
    - Returns `404` for unknown server labels.
    - Returns `409` when a known server is currently unavailable.
    - Returns `503` when the configured Built-in MCP runtime is unreachable.

______________________________________________________________________

### Error Responses

Errors follow the standard OpenAI error format.

```json
{
  "error": {
    "message": "Invalid model name",
    "type": "invalid_request_error",
    "param": "model",
    "code": "model_not_found"
  }
}
```

| HTTP Status | Error Type              | Description                                   |
| :---------- | :---------------------- | :-------------------------------------------- |
| 400         | `invalid_request_error` | Invalid input or parameters.                  |
| 401         | `authentication_error`  | Missing or invalid API key (if auth enabled). |
| 404         | `invalid_request_error` | Unknown `response_id` or model.               |
| 422         | `bad_input`             | Request contract validation failure.          |
| 500         | `api_error`             | Internal server error.                        |
