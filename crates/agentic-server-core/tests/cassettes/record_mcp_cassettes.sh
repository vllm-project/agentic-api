#!/usr/bin/env bash
# Records matching native MCP counter scenarios against the gateway and OpenAI.
#
# Each provider records five single-turn cassettes:
#   Streaming happy paths:
#     1. discover and list the counter tools without calling one
#     2. call sum and echo successfully
#   Streaming unhappy paths:
#     3. call sum without its required `b` argument
#     4. call sum with a string instead of an integer for `a`
#   Blocking happy path:
#     5. call say_hello successfully
#
# MCP_RECORD_CASE=approval records the `mcp-approval` items scenario of
# record_cassette.py instead: one non-streaming turn per `require_approval`
# form against the public Microsoft Learn MCP server, so no counter server is
# needed. Every turn is kept, including the ones a provider rejects; only the
# "never" turn must complete. The gateway must allow learn.microsoft.com
# (AGENTIC_MCP_ALLOWED_HOSTS=learn.microsoft.com).
#
# MCP resources are intentionally not recorded here. Agentic API implements
# the OpenAI Responses MCP contract (`tools/list` and `tools/call`); resource
# discovery and `resources/read` belong to MCP host applications.
#
# The default records both providers so every gateway cassette has an OpenAI
# ground-truth counterpart. OPENAI_API_KEY must be set for the default run.
#
# Recording disables MCP approval prompts, so the counter endpoint must be an
# explicitly trusted, operator-controlled server. There is intentionally no
# public endpoint default.
#
# Usage from the repository root:
#   COUNTER_MCP_SERVER_URL=https://your-counter.example/mcp \
#     bash crates/agentic-server-core/tests/cassettes/record_mcp_cassettes.sh
#   MCP_RECORD_SET=gateway \
#     GATEWAY_COUNTER_MCP_SERVER_URL=http://localhost:8000/mcp \
#     bash crates/agentic-server-core/tests/cassettes/record_mcp_cassettes.sh
#   MCP_RECORD_SET=openai \
#     OPENAI_MCP_SERVER_URL=https://your-counter.example/mcp \
#     bash crates/agentic-server-core/tests/cassettes/record_mcp_cassettes.sh
#   MCP_RECORD_CASE=approval \
#     bash crates/agentic-server-core/tests/cassettes/record_mcp_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPTS_DIR/../../../.." && pwd)"
BASE_DIR="$SCRIPTS_DIR/mcp"
TOOLS_FILE=""
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
MODEL="${MODEL:-Qwen/Qwen3.5-35B-A3B-FP8}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"
MCP_RECORD_SET="${MCP_RECORD_SET:-all}"
MCP_RECORD_CASE="${MCP_RECORD_CASE:-counter}"
COUNTER_SERVER_LABEL="${COUNTER_SERVER_LABEL:-counter}"
COUNTER_MCP_SERVER_URL="${COUNTER_MCP_SERVER_URL:-}"
GATEWAY_COUNTER_MCP_SERVER_URL="${GATEWAY_COUNTER_MCP_SERVER_URL:-$COUNTER_MCP_SERVER_URL}"
OPENAI_MCP_SERVER_URL="${OPENAI_MCP_SERVER_URL:-$COUNTER_MCP_SERVER_URL}"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4o}"
OPENAI_MODEL_SLUG="$(echo "$OPENAI_MODEL" | tr '/: ' '---')"
REPO_PLACEHOLDER="<AGENTIC_API_REPO>"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

validate_mcp_server_url() {
  local variable_name="$1"
  local server_url="$2"
  local require_https="$3"

  if [[ -z "$server_url" ]]; then
    echo "ERROR: $variable_name must be set to an explicitly trusted MCP endpoint" >&2
    exit 1
  fi

  python - "$variable_name" "$server_url" "$require_https" <<'PY'
import sys
from urllib.parse import urlparse

variable_name, server_url, require_https = sys.argv[1:]
parsed = urlparse(server_url)
errors = []
if parsed.scheme not in {"http", "https"}:
    errors.append("scheme must be http or https")
if not parsed.hostname:
    errors.append("hostname is required")
if parsed.username or parsed.password:
    errors.append("credentials must not be embedded in the URL")
if parsed.fragment:
    errors.append("fragments are not allowed")
if require_https == "true" and parsed.scheme != "https":
    errors.append("OpenAI recordings require a public HTTPS endpoint")
if errors:
    raise SystemExit(f"ERROR: invalid {variable_name}: {', '.join(errors)}")
PY

  if ! curl \
    --silent \
    --show-error \
    --output /dev/null \
    --head \
    --connect-timeout 10 \
    --max-time 15 \
    "$server_url"
  then
    echo "ERROR: $variable_name is not reachable: $server_url" >&2
    exit 1
  fi
}

sanitize_cassette() {
  local file="$1"
  perl -0pi -e "s|\\Q$REPO_ROOT\\E|$REPO_PLACEHOLDER|g" "$file"
}

validate_recorded_response() {
  local file="$1"
  local stream_flag="$2"

  python - "$file" "$stream_flag" <<'PY'
import json
import sys
from pathlib import Path

import yaml

path = Path(sys.argv[1])
streaming = sys.argv[2] == "--stream"
document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
turns = document.get("turns") or []
if len(turns) != 1:
    raise SystemExit(f"ERROR: expected one recorded turn in {path}, found {len(turns)}")

response = turns[0].get("response") or {}
status_code = response.get("status_code")
if status_code != 200:
    raise SystemExit(f"ERROR: recording returned HTTP {status_code}: {response.get('body')}")

if not streaming:
    body = response.get("body") or {}
    if not isinstance(body, dict) or body.get("status") != "completed":
        raise SystemExit(f"ERROR: blocking recording did not complete: {body}")
    raise SystemExit(0)

events = []
for raw in response.get("sse") or []:
    for line in raw.splitlines():
        if not line.startswith("data: ") or line == "data: [DONE]":
            continue
        try:
            events.append(json.loads(line.removeprefix("data: ")))
        except json.JSONDecodeError:
            continue

errors = [event.get("error") for event in events if event.get("type") == "error"]
if errors:
    raise SystemExit(f"ERROR: streaming recording returned an error event: {errors[0]}")
if not any(event.get("type") == "response.completed" for event in events):
    raise SystemExit("ERROR: streaming recording has no response.completed event")
PY
}

write_native_mcp_tools_file() {
  local server_url="$1"
  local allowed_tools="${2:-}"
  local allowed_tools_field=""

  if [[ -n "$allowed_tools" ]]; then
    allowed_tools_field="    \"allowed_tools\": $allowed_tools,"
  fi

  cat > "$TOOLS_FILE" <<JSON
[
  {
    "type": "mcp",
    "server_label": "$COUNTER_SERVER_LABEL",
    "server_url": "$server_url",
$allowed_tools_field
    "require_approval": "never"
  }
]
JSON
}

record_single_turn() {
  local endpoint_flag="$1"
  local endpoint="$2"
  local model="$3"
  local output="$4"
  local prompt="$5"
  local stream_flag="$6"
  local tool_choice="$7"
  local temporary_output

  temporary_output="$(mktemp "$BASE_DIR/.mcp-cassette.XXXXXX")"

  if ! printf '%s\n' "$prompt" \
    | python "$SCRIPTS_DIR/record_cassette.py" \
        --mode responses \
        --turns 1 \
        "$stream_flag" \
        --model "$model" \
        "$endpoint_flag" "$endpoint" \
        --tools "$TOOLS_FILE" \
        --tool-choice "$tool_choice" \
        --max-output-tokens 4096 \
        --output "$temporary_output"
  then
    rm -f -- "$temporary_output"
    return 1
  fi

  if ! validate_recorded_response "$temporary_output" "$stream_flag"; then
    rm -f -- "$temporary_output"
    return 1
  fi
  sanitize_cassette "$temporary_output"
  mv -- "$temporary_output" "$output"
  green "✓ MCP cassette recorded -> $output"
}

record_provider_suite() {
  local provider="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local model_slug="$5"
  local server_url="$6"
  local output_prefix="$7"
  local tool_name_prefix="$8"

  local list_output="$BASE_DIR/${output_prefix}-list-tools-${model_slug}-streaming.yaml"
  local call_output="$BASE_DIR/${output_prefix}-call-sum-and-echo-${model_slug}-streaming.yaml"
  local missing_argument_output="$BASE_DIR/${output_prefix}-sum-missing-argument-${model_slug}-streaming.yaml"
  local invalid_type_output="$BASE_DIR/${output_prefix}-sum-invalid-argument-type-${model_slug}-streaming.yaml"
  local blocking_output="$BASE_DIR/${output_prefix}-say-hello-${model_slug}-nonstreaming.yaml"

  bold "$provider MCP cassettes — native counter tools"
  bold "Endpoint: $endpoint"
  bold "Model:    $model"
  bold "Server:   $COUNTER_SERVER_LABEL"
  bold "MCP URL:  $server_url"

  bold "$provider streaming happy path 1/2 — tools/list"
  write_native_mcp_tools_file "$server_url"
  record_single_turn \
    "$endpoint_flag" "$endpoint" "$model" "$list_output" \
    "List every MCP tool available from the '${COUNTER_SERVER_LABEL}' server. Give each tool's name and a one-sentence description. Do not call any tool." \
    --stream auto

  bold "$provider streaming happy path 2/2 — two successful tools/call operations"
  write_native_mcp_tools_file "$server_url" '["sum", "echo"]'
  record_single_turn \
    "$endpoint_flag" "$endpoint" "$model" "$call_output" \
    "Call ${tool_name_prefix}sum exactly once with {\"a\":20,\"b\":22}. Then call ${tool_name_prefix}echo exactly once with {\"message\":\"mcp-contract\"}. Do not call any other tool. Finish with exactly SUM=42 ECHO=mcp-contract." \
    --stream required

  bold "$provider streaming unhappy path 1/2 — missing required argument"
  write_native_mcp_tools_file "$server_url" '["sum"]'
  record_single_turn \
    "$endpoint_flag" "$endpoint" "$model" "$missing_argument_output" \
    "Call ${tool_name_prefix}sum exactly once using the literal argument object {\"a\":40}. Do not add the missing b argument and do not correct the object. After the MCP call fails, report its error in one sentence. Do not retry." \
    --stream required

  bold "$provider streaming unhappy path 2/2 — invalid argument type"
  write_native_mcp_tools_file "$server_url" '["sum"]'
  record_single_turn \
    "$endpoint_flag" "$endpoint" "$model" "$invalid_type_output" \
    "Call ${tool_name_prefix}sum exactly once using the literal argument object {\"a\":\"not-an-integer\",\"b\":2}. Do not correct the argument type. After the MCP call fails, report its error in one sentence. Do not retry." \
    --stream required

  bold "$provider blocking happy path — one successful tools/call operation"
  write_native_mcp_tools_file "$server_url" '["say_hello"]'
  record_single_turn \
    "$endpoint_flag" "$endpoint" "$model" "$blocking_output" \
    "Call ${tool_name_prefix}say_hello exactly once with {}. Do not call any other tool. Return exactly MCP_SAYS=hello." \
    --no-stream required
}

case "$MCP_RECORD_SET" in
  gateway|openai|all) ;;
  *)
    echo "ERROR: MCP_RECORD_SET must be gateway, openai, or all" >&2
    exit 1
    ;;
esac

record_approval_case() {
  local provider="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local output="$5"
  local temporary_output

  bold "$provider MCP cassette — require_approval forms (Microsoft Learn MCP)"
  bold "Endpoint: $endpoint"
  bold "Model:    $model"
  temporary_output="$(mktemp "$BASE_DIR/.mcp-cassette.XXXXXX")"
  if ! python "$SCRIPTS_DIR/record_cassette.py" \
      --mode items \
      --items-scenario mcp-approval \
      --turns 8 \
      --no-stream \
      --model "$model" \
      "$endpoint_flag" "$endpoint" \
      --output "$temporary_output"
  then
    rm -f -- "$temporary_output"
    return 1
  fi
  if ! python - "$temporary_output" <<'PY'
import sys
from pathlib import Path

import yaml

path = Path(sys.argv[1])
turns = (yaml.safe_load(path.read_text(encoding="utf-8")) or {}).get("turns") or []
if len(turns) != 8:
    raise SystemExit(f"ERROR: expected 8 recorded turns in {path}, found {len(turns)}")
first = turns[0].get("response") or {}
body = first.get("body") or {}
if first.get("status_code") != 200 or not isinstance(body, dict) or body.get("status") != "completed":
    raise SystemExit(f"ERROR: the require_approval \"never\" turn did not complete: {body}")
PY
  then
    rm -f -- "$temporary_output"
    return 1
  fi
  sanitize_cassette "$temporary_output"
  mv -- "$temporary_output" "$output"
  green "✓ MCP approval cassette recorded -> $output"
}

if [[ "$MCP_RECORD_CASE" == "approval" ]]; then
  if [[ "$MCP_RECORD_SET" == "openai" || "$MCP_RECORD_SET" == "all" ]] && [[ -z "${OPENAI_API_KEY:-}" ]]; then
    echo "ERROR: OPENAI_API_KEY must be set for MCP_RECORD_SET=$MCP_RECORD_SET" >&2
    exit 1
  fi
  mkdir -p "$BASE_DIR"
  if [[ "$MCP_RECORD_SET" == "openai" || "$MCP_RECORD_SET" == "all" ]]; then
    record_approval_case OpenAI --openai https://api.openai.com "$OPENAI_MODEL" \
      "$BASE_DIR/mcp-approval-openai-${OPENAI_MODEL_SLUG}-nonstreaming.yaml"
  fi
  if [[ "$MCP_RECORD_SET" == "gateway" || "$MCP_RECORD_SET" == "all" ]]; then
    record_approval_case Gateway --gateway "$GATEWAY_URL" "$MODEL" \
      "$BASE_DIR/mcp-approval-gateway-${MODEL_SLUG}-nonstreaming.yaml"
  fi
  exit 0
elif [[ "$MCP_RECORD_CASE" != "counter" ]]; then
  echo "ERROR: MCP_RECORD_CASE must be counter or approval" >&2
  exit 1
fi

# Validate the OpenAI requirements before recording the gateway suite. This
# prevents a default `all` run from leaving only half of the cassette pairs.
if [[ "$MCP_RECORD_SET" == "openai" || "$MCP_RECORD_SET" == "all" ]]; then
  if [[ -z "${OPENAI_API_KEY:-}" ]]; then
    echo "ERROR: OPENAI_API_KEY must be set for MCP_RECORD_SET=$MCP_RECORD_SET" >&2
    exit 1
  fi
  validate_mcp_server_url OPENAI_MCP_SERVER_URL "$OPENAI_MCP_SERVER_URL" true
fi

if [[ "$MCP_RECORD_SET" == "gateway" || "$MCP_RECORD_SET" == "all" ]]; then
  validate_mcp_server_url \
    GATEWAY_COUNTER_MCP_SERVER_URL \
    "$GATEWAY_COUNTER_MCP_SERVER_URL" \
    false
fi

mkdir -p "$BASE_DIR"
TOOLS_FILE="$(mktemp "$BASE_DIR/.mcp-tools.XXXXXX.json")"
trap 'rm -f -- "$TOOLS_FILE"' EXIT

if [[ "$MCP_RECORD_SET" == "openai" || "$MCP_RECORD_SET" == "all" ]]; then
  record_provider_suite \
    OpenAI \
    --openai https://api.openai.com \
    "$OPENAI_MODEL" "$OPENAI_MODEL_SLUG" \
    "$OPENAI_MCP_SERVER_URL" \
    mcp-openai-reference-counter \
    "${COUNTER_SERVER_LABEL}__"
fi

if [[ "$MCP_RECORD_SET" == "gateway" || "$MCP_RECORD_SET" == "all" ]]; then
  record_provider_suite \
    Gateway \
    --gateway "$GATEWAY_URL" \
    "$MODEL" "$MODEL_SLUG" \
    "$GATEWAY_COUNTER_MCP_SERVER_URL" \
    mcp-gateway-counter \
    "mcp__${COUNTER_SERVER_LABEL}__"
fi
