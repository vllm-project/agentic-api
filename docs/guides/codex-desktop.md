# Codex Desktop with local models

Run the Codex desktop UI against a model served by vLLM through Agentic API. The desktop app and its Codex runtime
continue to handle local commands, file operations, approvals, and task history. Agentic API serves the Responses API
between that runtime and vLLM:

```text
Codex Desktop → Codex runtime → Agentic API → vLLM
```

This guide uses a separate desktop profile and Codex home, so you can keep your normal desktop session open. It
configures a [custom model provider](https://learn.chatgpt.com/docs/config-file/config-advanced#custom-model-providers)
and a static model catalog; no desktop app patch is needed.

## Tested configuration

The following combination was tested on Linux ARM64 on September 14, 2026:

| Component | Version or configuration |
|---|---|
| Desktop app | `26.901.51231`, installed as ChatGPT, using its **Codex** mode |
| Bundled Codex runtime | `0.153.4` |
| Agentic API | Commit `3bb55fd`, built from source |
| vLLM model | `RedHatAI/Qwen3-Coder-Next-NVFP4` |
| Upstream | `http://127.0.0.1:8000` |
| Gateway | `http://127.0.0.1:3020/v1` |

The desktop displayed the custom provider and model without an OpenAI login. A task executed shell commands, created
and read back a file, and completed a second turn that recalled and read the same file. This verifies the desktop
connection and client-executed tool loop. Native `apply_patch`, cancellation, long-context compaction, and task resume
after restarting the app were not validated as working.

The launch paths and environment variables below were verified with that Linux build. Other desktop releases and
operating systems may need different launch settings.

## Prerequisites

- An installed desktop app and its bundled Codex executable.
- A running vLLM deployment with a Responses-compatible endpoint and function calling enabled for the selected model.
- Bash, `curl`, `jq`, Python 3, and the repository's Rust toolchain.

These examples use an unauthenticated gateway and upstream on localhost. For authenticated deployments, configure
provider authentication separately; see [GitHub authentication with Dex](../deploying/github-oidc.md).

Run the commands below from the repository root in Bash. Set the paths for your installation and use the exact model
ID returned by the upstream's `/v1/models` endpoint:

```bash
export DESKTOP_STATE="$HOME/.local/share/agentic-api/codex-desktop"
export DESKTOP_BIN=/usr/bin/chatgpt
export CODEX_BIN=/usr/lib/chatgpt/resources/codex
export UPSTREAM_URL=http://127.0.0.1:8000
export GATEWAY_URL=http://127.0.0.1:3020
export MODEL=RedHatAI/Qwen3-Coder-Next-NVFP4

mkdir -p "$DESKTOP_STATE/codex-home" "$DESKTOP_STATE/app-data" "$DESKTOP_STATE/workspace"
"$CODEX_BIN" --version
curl --fail --silent --show-error "$UPSTREAM_URL/v1/models" | jq '.data[].id'
cargo build -p agentic-server --bin agentic-server
```

Use an absolute path for `DESKTOP_STATE`. Keep this directory separate from your normal Codex home and desktop profile;
the setup below writes its own `config.toml` and model catalog there. It persists across app launches.

## Start the gateway

In the same terminal, start Agentic API and leave it running:

```bash
env -u OPENAI_API_KEY -u V_API_KEY \
  DATABASE_URL="sqlite://$DESKTOP_STATE/gateway.db" \
  ./target/debug/agentic-server \
  --gateway-host 127.0.0.1 \
  --gateway-port 3020 \
  --llm-api-base "$UPSTREAM_URL"
```

If you change the gateway port, update `GATEWAY_URL` too. In another Bash terminal, repeat the `export` statements from
the prerequisites before running the remaining commands.

## Create the model catalog and provider configuration

Request the gateway's Codex model catalog using the version of the runtime bundled with the desktop app. Select only
the intended model and disable the incompatible freeform `apply_patch` declaration:

```bash
CLIENT_VERSION=$("$CODEX_BIN" --version | awk '{print $NF}')
set -o pipefail
curl --fail --silent --show-error \
  "$GATEWAY_URL/v1/models?client_version=$CLIENT_VERSION" |
  jq --exit-status --arg model "$MODEL" '
    {models: [.models[] | select(.slug == $model) | .apply_patch_tool_type = null]}
    | if (.models | length) == 1 then .
      else error("expected exactly one matching model in the gateway catalog") end
  ' > "$DESKTOP_STATE/codex-home/model_catalog.json"
```

Stop if that command fails. `model_catalog_json` supplies model metadata at startup without relying on the ordinary
model cache's expiration or refresh behavior. Keep the generated catalog at the configured path, and regenerate it
when changing the model or desktop runtime.

Generate the isolated configuration:

```bash
python3 - <<'PY'
import json
import os
from pathlib import Path

state = Path(os.environ["DESKTOP_STATE"])
config = "\n".join([
    "model = " + json.dumps(os.environ["MODEL"]),
    'model_provider = "agentic-api"',
    "model_catalog_json = " + json.dumps(str(state / "codex-home/model_catalog.json")),
    'model_reasoning_effort = "low"',
    'web_search = "disabled"',
    'sandbox_mode = "workspace-write"',
    'approval_policy = "on-request"',
    "",
    "[features]",
    "image_generation = false",
    "apps = false",
    "plugins = false",
    "",
    "[model_providers.agentic-api]",
    'name = "Agentic API (local desktop)"',
    "base_url = " + json.dumps(os.environ["GATEWAY_URL"].rstrip("/") + "/v1"),
    'wire_api = "responses"',
    "requires_openai_auth = false",
    "supports_websockets = true",
    "",
])
(state / "codex-home/config.toml").write_text(config)
PY
```

### Why disable `apply_patch`?

The gateway's default catalog advertises `apply_patch_tool_type: "freeform"`. The tested desktop runtime then declares
`apply_patch` as a grammar-constrained custom tool. The gateway's custom-tool normalization rejects that format before
inference with:

```text
tool error: invalid tool config: custom tool 'apply_patch' uses an unsupported format; gateway normalization cannot preserve constrained decoding
```

Setting `apply_patch_tool_type` to `null` omits that tool and allows shell-based file editing. Do not set it to
`"function"`: Codex `0.153.4` rejects that value. This is a compatibility workaround, not native `apply_patch` support.
It does not disable Codex's sandbox or approvals.

## Launch the separate desktop instance

```bash
env -u OPENAI_API_KEY -u OPENAI_BASE_URL -u CODEX_APP_SERVER_WS_URL \
  CODEX_HOME="$DESKTOP_STATE/codex-home" \
  CODEX_ELECTRON_USER_DATA_PATH="$DESKTOP_STATE/app-data" \
  CODEX_CLI_PATH="$CODEX_BIN" \
  CODEX_APP_SERVER_FORCE_CLI=1 \
  "$DESKTOP_BIN" --user-data-dir="$DESKTOP_STATE/app-data"
```

Both state directories matter: `CODEX_HOME` isolates the runtime configuration and task history;
`CODEX_ELECTRON_USER_DATA_PATH` isolates the desktop profile and allows a separate instance. `CODEX_CLI_PATH` pins the
runtime to the bundled binary, and `CODEX_APP_SERVER_FORCE_CLI=1` selects the local runtime instead of an existing
WebSocket app-server connection. These desktop environment variables are implementation-specific and may change
between releases.

Complete or skip first-run personalization and skip importing existing chats for this isolated setup. In **Codex** mode,
confirm that the profile menu shows **Agentic API (local desktop)** and the model picker shows your chosen model.

The configuration disables image generation, apps, plugins, and web search for the initial test. The desktop may
still install bundled plugins during startup; this setup does not establish their compatibility with the local model.

## Verify a task and follow-up turn

Prepare a fixture and print the workspace path:

```bash
printf 'Desktop integration test\n' > "$DESKTOP_STATE/workspace/README.md"
printf '%s\n' "$DESKTOP_STATE/workspace"
```

Start a new task in the isolated desktop window. Replace `<workspace>` with the printed absolute path:

```text
Work only in <workspace>. Use your shell tool to run pwd and read README.md there.
Then use a separate shell call to create desktop-smoke.txt containing exactly
DESKTOP_AGENTIC_OK followed by a newline, and read it back. Do not use apply_patch,
access other projects or credentials, use the network, or install anything.
After verifying the file, reply only DESKTOP_AGENTIC_OK.
```

Review any command approval prompts. The desktop should show the command activity and then `DESKTOP_AGENTIC_OK`.
Independently verify the file in your terminal:

```bash
printf 'DESKTOP_AGENTIC_OK\n' | cmp - "$DESKTOP_STATE/workspace/desktop-smoke.txt"
```

Send a follow-up in the same task:

```text
Use your shell tool to read the file you just created, using the path from our
previous turn. Do not modify anything. If it still contains DESKTOP_AGENTIC_OK,
reply only DESKTOP_CONTINUATION_OK.
```

Expected: another file-read operation and `DESKTOP_CONTINUATION_OK`. A plain greeting alone verifies generation but
does not exercise tool calls or continuation.

## Reopen and troubleshoot

Reuse the same state directories and launch command to reopen the isolated app. Stop the gateway with `Ctrl-C` when
finished; starting it again with the same database path preserves its stored responses.

| Symptom | Check |
|---|---|
| Normal app opens or the model is missing | Set both state-directory variables on the desktop process, check the bundled runtime path, and restart the isolated instance after changing its catalog. |
| `invalid tool config` mentioning `apply_patch` | Confirm the selected catalog entry has `apply_patch_tool_type: null` and `model_catalog_json` points to that file. |
| Model catalog generation fails | Check the gateway URL, runtime version, and exact model ID. Include the `client_version` query parameter to request Codex metadata. |
| Connection refused | Start the gateway and upstream; make sure the provider's `base_url` ends in `/v1` and uses the configured gateway port. |
| Catalog parse error after an app update | Regenerate the catalog using that app's bundled runtime version and check for model-metadata schema changes. |

For CLI-only setup and tests, see [Harness CLI Testing](harness-cli-testing.md).
