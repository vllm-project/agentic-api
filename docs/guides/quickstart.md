# Quickstart

Install vLLM Agentic API, connect it to a running vLLM server, and send your first Responses API request.
These commands use the [0.9.0 release on PyPI](https://pypi.org/project/agentic-api/0.9.0/).

## Before you start

- Use Python 3.10 or newer on Linux x86_64 (glibc 2.17 or newer) or macOS (Intel or Apple Silicon).
- Have vLLM serving a tool-capable model through its OpenAI-compatible `/v1/responses` endpoint at
  `http://127.0.0.1:8000`. Configure vLLM's tool-call parser for that model. The first request below does not call
  a tool. The base `agentic-api` package includes the Rust gateway and CLI, but does not install vLLM or GPU
  dependencies.

Check the upstream's model ID before making a request:

```bash
curl -sS http://127.0.0.1:8000/v1/models
```

Use the exact `id` returned for your model in the request below.

## Install the gateway

```bash
python -m pip install agentic-api==0.9.0
python -m agentic_api --version
python -m agentic_api doctor --mode remote
```

The `doctor` command checks the packaged gateway for this mode. It does not start vLLM.

## Start the gateway

In one terminal, run:

```bash
python -m agentic_api serve --vllm-base-url http://127.0.0.1:8000
```

The gateway listens on port 9000 by default. Leave this terminal running for the next step. Pass the upstream's
base URL, without `/v1`.

## Send a Responses API request

In another terminal, replace `MODEL_ID` with the ID returned by the upstream and run:

```bash
curl -sS http://127.0.0.1:9000/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","input":"Say hello in one sentence."}'
```

The JSON response contains an `output` array. To continue a stored response, see the
[Responses API reference](../api/index.md#responses).

## Next steps

- For a coding client, install Codex or Claude Code separately and use the packaged `agentic` CLI's `run codex` or
  `run claude` command. These commands launch a gateway for the client; see the
  [Rust CLI reference](../reference/rust-cli.md) and [Claude Code guide](harness-cli-testing.md).
- To let the Python launcher manage vLLM on a supported Linux GPU host, install
  `agentic-api[local]==0.9.0` and use `python -m agentic_api serve --model MODEL_ID`. See
  [Python installation and workflows](python-installation.md) for the local mode and diagnostics.
- If you prefer Cargo, [agentic-server on crates.io](https://crates.io/crates/agentic-server)
  installs the Rust-native `agentic` CLI and gateway.
