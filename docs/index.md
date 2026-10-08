---
hide:
  - navigation
  - toc
---

# Welcome to vLLM Agentic API

<p style="text-align:center">
<strong>The stateful and agentic API layer for vLLM</strong>
</p>

<p style="text-align:center">
<script async defer src="https://buttons.github.io/buttons.js"></script>
<a class="github-button" href="https://github.com/vllm-project/agentic-api" data-show-count="true" data-size="large" aria-label="Star">Star</a>
<a class="github-button" href="https://github.com/vllm-project/agentic-api/subscription" data-show-count="true" data-icon="octicon-eye" data-size="large" aria-label="Watch">Watch</a>
<a class="github-button" href="https://github.com/vllm-project/agentic-api/fork" data-show-count="true" data-icon="octicon-repo-forked" data-size="large" aria-label="Fork">Fork</a>
</p>

vLLM Agentic API provides the stateful APIs needed for real-world agentic applications — managing conversations, tool calls, and multi-turn interactions on top of [vLLM](https://github.com/vllm-project/vllm)'s high-throughput inference engine.

!!! important

    This project is in early development. Follow along and contribute on [GitHub](https://github.com/vllm-project/agentic-api).

## Agentic APIs

Agentic API implements the OpenAI-compatible [Responses API](https://platform.openai.com/docs/api-reference/responses)
and serves the Anthropic Messages API for Claude Code. We validate the Responses implementation against the
[Open Responses](https://www.openresponses.org/) compatibility test suite.

- **Stateful conversations** — The server manages conversation history via `previous_response_id` or the Conversations API, eliminating client-side message tracking
- **Server-side tool execution** — Web search, MCP tools, and an opt-in embedded code interpreter run inside the gateway, with the model automatically executing multi-step tool chains
- **Multi-agent orchestration** — Stored HTTP Responses requests can spawn and coordinate subagents server-side
- **Streaming** — Server-sent events and WebSocket transports with structured lifecycle events
- **Compaction** — Automatic and explicit context compaction for long-running sessions
- **Compatibility tested** — Validated against the open Responses API compatibility test suite and replay recordings of vLLM, SGLang, and NVIDIA Dynamo traffic

See the [API reference](api/index.md) for the full endpoint list.

## Python Distribution

The `agentic-api` wheel packages the Rust gateway and a small Python launcher. Use the base package for proxy-only
installations, and the `[local]` extra when you want the launcher to manage a local vLLM process.

- [Quickstart](guides/quickstart.md) to install the published package, connect an upstream, and send a first request
- Version 0.9.0 is [published on PyPI](https://pypi.org/project/agentic-api/0.9.0/) for Linux x86_64, macOS Intel, and macOS Apple Silicon
- [Python installation and workflows](guides/python-installation.md) for PyPI and uvx installs, workflow artifact testing, `doctor`, and known-good model profiles
- The Rust-native `agentic` CLI remains supported for `serve`, `run codex`, `run claude`, and `validate`
- vLLM is a supported backend, not part of the Agentic API product name

## Why Agentic API?

vLLM is fast with state-of-the-art serving throughput, PagedAttention, continuous batching, and broad hardware support. But building agentic applications on top of it today requires significant client-side orchestration — managing conversation state, tool call loops, and multi-turn flows.

Agentic API moves that complexity server-side, so you can:

- **Drop in a single API call** instead of building multi-turn orchestration
- **Let the server manage state** instead of tracking conversation history client-side
- **Use familiar APIs** — OpenAI-compatible endpoints backed by vLLM's inference engine
