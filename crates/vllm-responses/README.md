# vllm-responses

`vllm-responses` defines typed, transport-independent inference events for
building OpenAI-compatible Responses API integrations.

It intentionally has no HTTP, SSE, persistence, or model-engine dependency.
Inference adapters emit [`InferenceEvent`] values; a Responses runtime owns
validation, response assembly, and client delivery.

## Lifecycle

An adapter emits `Started`, `InProgress`, zero or more complete output-item
lifecycles, then exactly one terminal event. Each active text, reasoning, or
function-call item must emit its matching completion event before `Completed`,
`Incomplete`, or `Failed`.

The initial profile supports one text part and one reasoning part per output
item. It carries prompt-cache, output, and reasoning usage through
`InferenceUsage`.

See the Agentic API shared-library design for the intended vLLM and Agentic
integration boundary.
