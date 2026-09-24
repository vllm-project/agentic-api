#!/usr/bin/env bash
# Records the five-profile code-interpreter public-contract matrix.
#
# HTTP profiles use the standard recording proxy. The gateway WebSocket
# profile uses record_cassette.py's bounded direct capture. Every recording is
# staged and semantically validated before it replaces a checked-in cassette.

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/code_interpreter"
PROMPTS="$BASE_DIR/prompts.txt"
OPENAI_TOOLS="$BASE_DIR/openai_tools.json"
GATEWAY_TOOLS="$BASE_DIR/gateway_tools.json"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-5.6}"
GATEWAY_MODEL="${GATEWAY_MODEL:-${MODEL:-Qwen/Qwen3.6-35B-A3B}}"
GATEWAY_URL="${GATEWAY_URL:-}"
CODE_INTERPRETER_RECORD_SET="${CODE_INTERPRETER_RECORD_SET:-all}"

model_slug() {
  printf '%s' "$1" | tr '/: ' '---'
}

assert_sanitized() {
  local cassette="$1"
  CASSETTE_PATH="$cassette" python -c '
import os
from pathlib import Path

import yaml

secret = os.environ.get("OPENAI_API_KEY", "")
text = Path(os.environ["CASSETTE_PATH"]).read_text(encoding="utf-8")
if secret and secret in text:
    raise SystemExit("recorded cassette contains OPENAI_API_KEY material")
cassette = yaml.safe_load(text)
for turn in cassette.get("turns", []):
    authorization = turn.get("request", {}).get("headers", {}).get("authorization")
    if authorization is not None and authorization != "Bearer ***":
        raise SystemExit("recorded cassette contains an unmasked authorization header")
'
}

record_profile() {
  local provider="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local tools="$5"
  local filename="$6"
  shift 6

  local temporary_output
  temporary_output="$(mktemp "$STAGING_DIR/.code-interpreter-cassette.XXXXXX")"
  if ! python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --no-store \
    --model "$model" \
    "$endpoint_flag" "$endpoint" \
    --tools "$tools" \
    --tool-choice required \
    --parallel-tool-calls false \
    --max-output-tokens 2048 \
    --output "$temporary_output" \
    "$@" < "$PROMPTS"
  then
    rm -f -- "$temporary_output"
    return 1
  fi
  assert_sanitized "$temporary_output"
  mv -- "$temporary_output" "$STAGING_DIR/$filename"
  RECORDED_FILES+=("$filename")
  printf 'staged %s %s\n' "$provider" "$filename"
}

record_http_pair() {
  local provider="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local tools="$5"
  local prefix="$6"
  local slug
  slug="$(model_slug "$model")"

  record_profile "$provider blocking" "$endpoint_flag" "$endpoint" "$model" "$tools" \
    "${prefix}-${slug}-nonstreaming.yaml" --no-stream
  record_profile "$provider HTTP/SSE" "$endpoint_flag" "$endpoint" "$model" "$tools" \
    "${prefix}-${slug}-streaming.yaml" --stream
}

case "$CODE_INTERPRETER_RECORD_SET" in
  openai-reference|openai|gateway-nonstreaming|gateway-streaming|gateway-websocket|gateway|all) ;;
  *)
    printf 'ERROR: CODE_INTERPRETER_RECORD_SET must be openai-reference, gateway-nonstreaming, gateway-streaming, gateway-websocket, gateway, or all\n' >&2
    exit 1
    ;;
esac

for required_file in "$PROMPTS" "$OPENAI_TOOLS" "$GATEWAY_TOOLS"; do
  if [[ ! -f "$required_file" ]]; then
    printf 'ERROR: required fixture does not exist: %s\n' "$required_file" >&2
    exit 1
  fi
done

if [[ "$CODE_INTERPRETER_RECORD_SET" =~ ^(openai-reference|openai|all)$ ]] && [[ -z "${OPENAI_API_KEY:-}" ]]; then
  printf 'ERROR: OPENAI_API_KEY is required for %s\n' "$CODE_INTERPRETER_RECORD_SET" >&2
  exit 1
fi
if [[ "$CODE_INTERPRETER_RECORD_SET" =~ ^(gateway-nonstreaming|gateway-streaming|gateway-websocket|gateway|all)$ ]] && [[ -z "$GATEWAY_URL" ]]; then
  printf 'ERROR: GATEWAY_URL is required for %s\n' "$CODE_INTERPRETER_RECORD_SET" >&2
  exit 1
fi

STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/agentic-code-interpreter-cassettes.XXXXXX")"
trap 'rm -rf -- "$STAGING_DIR"' EXIT
RECORDED_FILES=()
cp -a -- "$BASE_DIR/." "$STAGING_DIR/"

if [[ "$CODE_INTERPRETER_RECORD_SET" =~ ^(openai-reference|openai|all)$ ]]; then
  record_http_pair OpenAI --openai https://api.openai.com "$OPENAI_MODEL" "$OPENAI_TOOLS" \
    code-interpreter-openai-reference
fi

gateway_slug="$(model_slug "$GATEWAY_MODEL")"
if [[ "$CODE_INTERPRETER_RECORD_SET" =~ ^(gateway-nonstreaming|gateway|all)$ ]]; then
  record_profile "gateway blocking" --gateway "$GATEWAY_URL" "$GATEWAY_MODEL" "$GATEWAY_TOOLS" \
    "code-interpreter-gateway-${gateway_slug}-nonstreaming.yaml" --no-stream
fi
if [[ "$CODE_INTERPRETER_RECORD_SET" =~ ^(gateway-streaming|gateway|all)$ ]]; then
  record_profile "gateway HTTP/SSE" --gateway "$GATEWAY_URL" "$GATEWAY_MODEL" "$GATEWAY_TOOLS" \
    "code-interpreter-gateway-${gateway_slug}-streaming.yaml" --stream
fi
if [[ "$CODE_INTERPRETER_RECORD_SET" =~ ^(gateway-websocket|gateway|all)$ ]]; then
  record_profile "gateway WebSocket" --gateway "$GATEWAY_URL" "$GATEWAY_MODEL" "$GATEWAY_TOOLS" \
    "code-interpreter-gateway-${gateway_slug}-websocket.yaml" --stream --transport websocket
fi

printf 'Validating the five-profile code-interpreter public-contract matrix\n'
CODE_INTERPRETER_CASSETTE_DIR="$STAGING_DIR" \
  cargo test --manifest-path "$SCRIPTS_DIR/../../../../Cargo.toml" \
    -p agentic-server-core --test code_interpreter_characterization_test

for filename in "${RECORDED_FILES[@]}"; do
  chmod 664 "$STAGING_DIR/$filename"
  mv -- "$STAGING_DIR/$filename" "$BASE_DIR/$filename"
  printf 'recorded %s\n' "$BASE_DIR/$filename"
done
