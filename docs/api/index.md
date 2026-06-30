# API Reference

## Responses

### `POST /v1/responses`

HTTP Responses requests use the OpenAI-compatible Responses shape. Requests
with `store=true`, `previous_response_id`, or `conversation_id` run through the
stateful executor. Stateless `store=false` requests without continuation state
are proxied directly to the configured vLLM backend.

### `WS /v1/responses`

The same path accepts WebSocket upgrades for Codex-style Responses
continuations. Send one JSON text frame per turn:

```json
{
  "type": "response.create",
  "model": "test-model",
  "input": [{"type": "message", "role": "user", "content": "hi"}],
  "previous_response_id": "resp_optional",
  "store": true,
  "stream": true
}
```

The server normalizes the frame into the internal Responses request model and
uses the same response-store continuation path as HTTP. WebSocket replies are
JSON Responses stream events, including `response.created`,
`response.output_item.added`, `response.output_text.delta`, and
`response.completed`.

Invalid requests are returned as JSON WebSocket error events:

```json
{
  "type": "error",
  "status": 404,
  "error": {
    "message": "human-readable error details",
    "type": "not_found",
    "code": "not_found"
  }
}
```
