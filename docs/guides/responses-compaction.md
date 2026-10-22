# Responses compaction

Compaction turns a long Responses item history into a smaller, reusable context checkpoint. vLLM Agentic API supports
both the standalone `POST /v1/responses/compact` endpoint and automatic compaction on `POST /v1/responses`.

## Standalone compaction

Send direct input, a stored previous response ID, or both:

```bash
curl http://localhost:8000/v1/responses/compact \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "served-model",
    "input": [
      {"role": "user", "content": "Remember the deployment constraints."},
      {"role": "assistant", "content": "I will preserve them."}
    ]
  }'
```

The endpoint makes a non-streaming inference call to summarize the resolved item history. Its response has
`object: "response.compaction"`; `usage` describes that summary call. The `output` array is the canonical compacted
window: normalized user messages followed by exactly one `compaction` item.

```json
{
  "id": "resp_...",
  "object": "response.compaction",
  "created_at": 1784740000,
  "output": [
    {
      "type": "message",
      "id": "msg_...",
      "role": "user",
      "status": "completed",
      "content": "Remember the deployment constraints."
    },
    {
      "type": "compaction",
      "id": "cmp_...",
      "encrypted_content": "The user asked that the deployment constraints be preserved..."
    }
  ],
  "usage": {
    "input_tokens": 1200,
    "output_tokens": 180,
    "total_tokens": 1380,
    "input_tokens_details": {"cached_tokens": 0},
    "output_tokens_details": {"reasoning_tokens": 0}
  }
}
```

Pass the complete `output` array back as later Responses input. You may append new input items after it:

```json
{
  "model": "served-model",
  "input": [
    {"type": "message", "id": "msg_...", "role": "user", "status": "completed", "content": "..."},
    {"type": "compaction", "id": "cmp_...", "encrypted_content": "..."},
    {"role": "user", "content": "Continue the task."}
  ],
  "store": false
}
```

When response storage is configured, the returned response ID is also a continuation checkpoint and can be supplied
as `previous_response_id`. Compatibility fields such as `tools`, `parallel_tool_calls`, `reasoning`, and `text` are
accepted by the compact endpoint but are not used by the summary call.

## Automatic context management

Add a compaction policy to an ordinary Responses request:

```json
{
  "model": "served-model",
  "input": [{"role": "user", "content": "Long-running task context"}],
  "context_management": [
    {"type": "compaction", "compact_threshold": 120000}
  ]
}
```

The gateway first rehydrates any conversation or previous-response history. Before every inference round, including
rounds after gateway-managed tool output, it estimates the model-facing input size and compacts when that estimate is
greater than `compact_threshold`. Inference then continues using the compacted window. `context_management` is local
executor configuration and is not sent to vLLM. If compaction runs, the response usage is the saturating sum of every
summary call and all answer/tool-loop inference rounds.

The estimate is deterministic but is not a model-specific tokenizer. Textual fields are aggregated as UTF-8 bytes,
divided by four and rounded up, with fixed allowances for Responses item and content-part framing. Each image receives
a fixed 1,024-token allowance, independent of its URL, inline base64 size, dimensions, or `detail` setting. Actual
vision-token usage depends on the model and image processor, so choose a threshold with headroom. An entry without
`compact_threshold` is ignored because this server has no global default threshold.

## File inputs

Message content parts with `type: "input_file"` are parsed without dropping `file_data`, `file_id`, `file_url`,
or `filename`. The typed Responses executor accepts a message file only when `file_data` carries an inline UTF-8
text file, sent as either:

- a base64 data URL with a text media type: `text/*`, `application/json`, `application/xml`, `application/yaml`,
  any `+json` or `+xml` type such as `application/ld+json`, or a text `application/*` type such as
  `application/javascript`, `application/typescript`, `application/x-sh`, `application/sql`, or
  `application/toml`. For example, `data:text/plain;base64,aGVsbG8=`. Media type parameters such as `charset` are
  ignored. A text media type is accepted with or without a filename; or
- raw base64, or a data URL with any other media type except `application/pdf`, `image/*`, `audio/*`, and
  `video/*`, together with a text `filename`: one whose extension is a common text, config, or source code type
  such as `.txt`, `.md`, `.csv`, `.json`, `.yaml`, `.toml`, `.log`, `.py`, `.rb`, `.cs`, `.tsx`, or `.ps1`, or a
  conventional name such as `Dockerfile`, `Makefile`, `LICENSE`, `.gitignore`, or `.env`. `video/mp2t`, which
  some clients send for TypeScript, is accepted with a `.ts`, `.mts`, or `.cts` filename.

Content that passes these checks must still decode as UTF-8, so binary data is rejected even with a text filename.

The file is decoded only in the request sent to the model, where it becomes an `input_text` part in the same
position. When `filename` is set, the text starts with a `File: <filename>` line, so this part:

```json
{"type": "input_file", "filename": "notes.txt", "file_data": "data:text/plain;base64,aGVsbG8="}
```

reaches the model as:

```json
{"type": "input_text", "text": "File: notes.txt\nhello"}
```

Stored response and conversation history and compacted windows keep the client's original `input_file`; the text
is decoded again on each continuation. The file counts toward the request body limit
(`max_request_body_size_bytes`) in its base64 form, and the decoded text counts toward the model's context length.

Every other message file is rejected with HTTP 400 `invalid_request_error` (or the equivalent WebSocket error)
before inference and before anything is stored. The message gives the reason without echoing the file: `file_id`
(this gateway has no Files API), `file_url` (the gateway does not fetch files), a missing `file_data`, a non-text
file such as a PDF, a data URL that is not base64-encoded, invalid base64, or content that is not valid UTF-8.
Extract text from PDFs and other documents client-side and send it as `input_text`, or send pages as
`input_image`. The same validation applies to files restored from response/conversation history, explicit and
automatic compaction, `compaction_trigger`, and WebSocket `generate: false` requests.

HTTP `store: false` requests use the same typed executor, inline text-file normalization, and file-input validation.
Without `previous_response_id` or `conversation`, clients supply the complete item history in `input`; the gateway
does not load or persist state.
Structured function/custom tool call outputs containing files retain their existing pass-through behavior.

API `input_file` content is distinct from mentioning a local filename in Codex or asking a client-executed read tool
to read a file. A local file mention is not a file upload, and this gateway does not resolve local paths or implement
a file-ID resolver, OCR, or document parser.

## Local plaintext limitation

The wire field is named `encrypted_content` for Responses compatibility, but summaries generated by this local
implementation are **plaintext and are not encrypted**. Treat the field as conversation content when logging,
storing, transmitting, or applying data-retention controls. The gateway converts it to assistant context only for the
model-facing vLLM request; the public and stored item remains a `compaction` item.

OpenAI-generated encrypted compaction items are not decryptable or interoperable with this implementation. If one is
submitted, its `encrypted_content` bytes are treated literally as plaintext assistant context.
