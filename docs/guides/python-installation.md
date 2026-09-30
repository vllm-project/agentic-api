# Python installation and workflows

`agentic-api` is the Python distribution for the Rust-backed Agentic API gateway. vLLM is a supported inference
backend, not part of the Agentic API product name. Use the base wheel when you want a proxy-only install, and add the
`[local]` extra when you want the launcher to manage a local vLLM process.

The Rust-native `agentic` CLI remains supported for `run codex`, `run claude`, `serve`, and `validate`.

## Install from PyPI

Version 0.9.0 is [published on PyPI](https://pypi.org/project/agentic-api/0.9.0/). Use Python 3.10 or newer.
The [Python CLI reference](https://vllm-project.github.io/agentic-api/docs/latest/python-cli) lists commands, options, and defaults generated from the parser.

### Install the base package

The base wheel bundles the Rust executables and does not install vLLM:

```bash
python -m pip install agentic-api==0.9.0
agentic-api serve --vllm-base-url http://existing-vllm:8000
```

Use this mode when an upstream inference server is already running. Every Python launcher command also works with
`python -m agentic_api` in place of `agentic-api`.

### Install the local extra

On supported Linux GPU hosts, the local extra adds the tested vLLM dependency:

```bash
python -m pip install "agentic-api[local]==0.9.0"
agentic-api serve --model Qwen/Qwen3-30B-A3B-FP8
```

The launcher accepts arbitrary `--model` values. Choose a model and serving configuration suitable for your hardware.
Managed vLLM supports extra arguments after `--`:

```bash
agentic-api serve --model Qwen/Qwen3-30B-A3B-FP8 -- \
  --dtype bfloat16 \
  --max-model-len=32768
```

The launcher manages `--host`, `--port`, and `--api-key`; do not pass those vLLM options after `--`.
Use `--vllm-port` and the API-key environment-variable options on the launcher instead.

### Run with uvx

With uv installed, run the packaged Rust CLI in an isolated environment without a global installation.
Install Codex or Claude Code separately and connect to an existing inference server:

```bash
uvx --from agentic-api==0.9.0 agentic --version
uvx --from agentic-api==0.9.0 agentic run codex --upstream http://existing-vllm:8000
uvx --from agentic-api==0.9.0 agentic run claude --upstream http://existing-vllm:8000
uvx --from agentic-api==0.9.0 agentic serve --upstream http://existing-vllm:8000
```

### Install a workflow artifact

To test a wheel before publication, download the artifact for your platform from the release workflow:

```bash
python -m pip install /absolute/path/to/agentic_api-PLATFORM.whl
```

## Check the install

`doctor` reports whether the packaged Rust executable is present, whether the tested local vLLM wheel is installed, and
whether the current mode is healthy.

With no mode selected, `doctor` reports both local and remote health but uses remote health for its exit status, so the
base proxy-only install is considered healthy when its packaged gateway is available.

```bash
agentic-api doctor
agentic-api doctor --mode remote
agentic-api doctor --mode local
agentic-api doctor --mode remote --json
```

Use `--mode remote` when you only need the packaged Rust gateway checks. Use `--mode local` when you want to verify the
tested vLLM runtime and executable are available.

## Rust-native CLI usage

The Python package does not replace the Rust CLI. It complements it.

```bash
agentic run codex --model MODEL_ID
agentic run claude --model SERVED_MODEL_ALIAS
```

## Known-good model profiles

The matrix below is documentation data, not an allowlist. `agentic-api serve` still accepts arbitrary `--model`
values. The served alias column is only needed when Claude Code requires a slash-free model name, and the alias values
here are examples that should be revalidated on the target Linux GPU before promotion.

| Model identifier | Required hardware class | Served alias for Claude Code | Tested launch arguments |
| --- | --- | --- | --- |
| `Qwen/Qwen3-30B-A3B-FP8` | Linux GPU host that can serve a 30B FP8 model | `qwen3-30b-a3b-fp8` | `vllm serve Qwen/Qwen3-30B-A3B-FP8 --reasoning-parser deepseek_r1 --port 5050` and `vllm serve Qwen/Qwen3-30B-A3B-FP8 --tool-call-parser hermes --enable-auto-tool-choice --port 5050` |

Other documented model IDs already exercised in this repository include `Qwen/Qwen3.5-35B-A3B-FP8` and
`Qwen/Qwen3.8-27B-FP8`. Treat them as examples pending hardware revalidation rather than as a CLI allowlist.

## Publishing wheels (maintainers)

See the [release guide](../developing/releases.md) for version preparation, GitHub Actions UI and CLI instructions,
PyPI Trusted Publishing configuration, the required wheel matrix, registry verification, and recovery from failed or
partial uploads. PRs and merges build and validate wheels. Trigger the release workflow on `main` to have GitHub
Actions validate and publish them.
