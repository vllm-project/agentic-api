# vLLM Responses

<div style="display: flex; gap: 0.5rem; margin-bottom: 1rem;">
  <a href="https://www.openresponses.org/specification" style="text-decoration: none;">
    <img src="https://img.shields.io/badge/OpenResponses-Compliant-green?style=flat-square" alt="OpenResponses Compliant">
  </a>
</div>

**FastAPI gateway for the OpenAI-style Responses API.**

`vLLM Responses` sits in front of a vLLM server and transforms its standard Chat Completions output into the rich, stateful **Responses API** format. It gives you advanced capabilities like server-side tool execution and conversation state management without modifying your inference backend.

______________________________________________________________________

## Why use this gateway?

- **Stateful Conversations**: Maintain conversation history automatically using `previous_response_id`, backed by a persistent store (SQLite/Postgres).
- **Built-in Code Interpreter**: Let the model write and execute code in a sandboxed environment on the gateway.
- **Built-in Web Search**: Let the model search the web, open pages, and search within page text through the gateway-owned `web_search` tool.
- **MCP Integration (Built-in MCP + Remote MCP)**: Use configured Built-in MCP servers or Remote MCP declarations with Responses-style streaming events.
- **Correct SSE Streaming**: Receive spec-compliant Server-Sent Events with precise ordering and shape guarantees.
- **Drop-in Compatibility**: Works with any standard vLLM OpenAI-compatible endpoint.

## Getting Started

<div class="grid cards" markdown>

- :rocket: **[Quickstart](getting-started/quickstart.md)** <br>Get up and running in 5 minutes.

- :material-download: **[Installation](getting-started/installation.md)** <br>Install the CLI and dependencies.

- :material-server: **[Running the Gateway](usage/running-the-gateway.md)** <br>Deploy alongside vLLM or as a standalone service.

- :material-school: **[Architecture](getting-started/architecture.md)** <br>Understand how the gateway works.

</div>

## Documentation Map

- **[Usage](usage/running-the-gateway.md)**: Choose a runtime mode and get the gateway running before diving into flags.
- **[Features](features/statefulness.md)**: Deep dive into statefulness, built-in tools, and MCP integration.
- **[Reference](reference/api-reference.md)**: API endpoint details, configuration variables, and event schemas.
- **[Deployment](deployment/configuration.md)**: Production configuration.
- **[Examples](examples/code-interpreter-examples.md)**: Code snippets for code interpreter, MCP usage, and tool loops.

______________________________________________________________________

!!! tip "New to the Responses API?"

    Start with the **[Quickstart](getting-started/quickstart.md)** to see the API in action, then check out **[Architecture](getting-started/architecture.md)** to understand the concepts.
