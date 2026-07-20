#!/usr/bin/env bash
set -euo pipefail

# record_codex_cli_tool_call_cassettes.sh
#
# Records YAML replay cassettes for Codex CLI-shaped tool calls.
#
# Default matrix:
#   - gateway HTTP/SSE: function + Codex namespace + custom tools
#   - gateway WebSocket: function + Codex namespace + custom tools
#   - direct vLLM HTTP/SSE: function + flattened namespace function + custom tool
#   - direct OpenAI HTTPS/SSE: function + Codex namespace + custom tools
#   - direct OpenAI WebSocket: function + Codex namespace + custom tools
#
# Direct vLLM expects the flattened function shape. Set VLLM_URL or V_API_BASE
# explicitly before recording direct vLLM cassettes.

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/codex"

PYTHON="${PYTHON:-python3}"
RECORDER="${RECORDER:-${SCRIPTS_DIR}/record_cassette.py}"
TOOLS_DIR="${TOOLS_DIR:-${BASE_DIR}/tools}"
OUT="${OUT:-${BASE_DIR}}"

GATEWAY_URL="${GATEWAY_URL:-http://127.0.0.1:3018}"

VLLM_URL="${VLLM_URL:-${V_API_BASE:-}}"
V_MODEL="${V_MODEL:-${MODEL:-Qwen/Qwen3.6-35B-A3B}}"
GATEWAY_MODEL="${GATEWAY_MODEL:-$V_MODEL}"
GATEWAY_CASSETTE_MODEL="${GATEWAY_CASSETTE_MODEL:-$V_MODEL}"

OPENAI_URL="${OPENAI_URL:-https://api.openai.com}"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4o}"
OPENAI_CUSTOM_MODEL="${OPENAI_CUSTOM_MODEL:-gpt-5.6}"

TOOL_TURNS="${TOOL_TURNS:-2}"
PROXY_PORT_BASE="${PROXY_PORT_BASE:-7070}"
TARGET="${1:-all}"

FUNCTION_TOOL="${TOOLS_DIR}/function_tool.json"
NAMESPACE_TOOL="${TOOLS_DIR}/namespace_tool.json"
CUSTOM_TOOL="${TOOLS_DIR}/custom_tool.json"
DIRECT_VLLM_FLAT_NAMESPACE_TOOL="${TOOLS_DIR}/direct_vllm_flat_namespace_tool.json"
TOOL_OUTPUTS="${TOOLS_DIR}/tool_outputs.json"

model_slug() {
  printf '%s\n' "$1" | tr '/: ' '---'
}

GATEWAY_MODEL_SLUG="$(model_slug "$GATEWAY_CASSETTE_MODEL")"
V_MODEL_SLUG="$(model_slug "$V_MODEL")"
OPENAI_MODEL_SLUG="$(model_slug "$OPENAI_MODEL")"
OPENAI_CUSTOM_MODEL_SLUG="$(model_slug "$OPENAI_CUSTOM_MODEL")"

next_proxy_port="$PROXY_PORT_BASE"

usage() {
  cat <<USAGE
Usage: $(basename "$0") [target]

Targets:
  all                  all cassettes used by Codex cassette tests
  gateway              gateway-http + gateway-ws
  gateway-http         gateway HTTP/SSE function + namespace + custom
  gateway-ws           gateway WebSocket function + namespace + custom
  gateway-custom       gateway HTTP/SSE + WebSocket custom only
  gateway-http-custom  gateway HTTP/SSE custom only
  gateway-ws-custom    gateway WebSocket custom only
  direct-vllm          same as direct-vllm-http
  direct-vllm-http     direct vLLM HTTP/SSE function + flattened namespace + custom
  direct-vllm-custom   direct vLLM HTTP/SSE custom only
  direct-vllm-ws       direct vLLM WebSocket function + flattened namespace
  openai               same as openai-https
  openai-https         direct OpenAI HTTPS/SSE function + custom
  openai-ws            direct OpenAI WebSocket function + custom
  openai-custom        direct OpenAI HTTPS/SSE + WebSocket custom only
  openai-https-custom  direct OpenAI HTTPS/SSE custom only
  openai-ws-custom     direct OpenAI WebSocket custom only
  openai-namespace     direct OpenAI HTTPS/SSE raw namespace, if accepted
  openai-ws-namespace  direct OpenAI WebSocket raw namespace, if accepted
  experimental-all     all plus direct-vllm-ws

Environment:
  OUT                    output cassette directory, default: ${OUT}
  GATEWAY_URL            gateway base URL, default: ${GATEWAY_URL}
  GATEWAY_MODEL          gateway-facing model name, default: ${GATEWAY_MODEL}
  GATEWAY_CASSETTE_MODEL model name used in gateway YAML filenames, default: ${GATEWAY_CASSETTE_MODEL}
  VLLM_URL or V_API_BASE direct vLLM base URL, required for direct-vllm* and all
  V_MODEL or MODEL       direct vLLM model, default: ${V_MODEL}
  OPENAI_URL             OpenAI base URL, default: ${OPENAI_URL}
  OPENAI_MODEL           OpenAI model, default: ${OPENAI_MODEL}
  OPENAI_CUSTOM_MODEL    OpenAI custom-tool model, default: ${OPENAI_CUSTOM_MODEL}
  OPENAI_API_KEY         required for openai* targets
  TOOL_TURNS             1 or 2, default: ${TOOL_TURNS}
  PROXY_PORT_BASE        first embedded recorder proxy port, default: ${PROXY_PORT_BASE}
USAGE
}

require_file() {
  local path="$1"
  if [[ ! -f "$path" ]]; then
    echo "error: missing file: ${path}" >&2
    exit 2
  fi
}

require_openai_key() {
  if [[ -z "${OPENAI_API_KEY:-}" ]]; then
    echo "error: OPENAI_API_KEY is required for OpenAI targets" >&2
    exit 2
  fi
}

require_direct_vllm_url() {
  if [[ -z "$VLLM_URL" ]]; then
    echo "error: VLLM_URL or V_API_BASE is required for direct vLLM cassette targets" >&2
    echo 'hint: set VLLM_URL="http://host:port" before running direct-vllm*, all, or experimental-all' >&2
    exit 2
  fi
}

alloc_proxy_port() {
  local port="$next_proxy_port"
  next_proxy_port=$((next_proxy_port + 1))
  printf '%s\n' "$port"
}

emit_prompts() {
  local first_prompt="$1"
  local second_prompt="$2"

  case "$TOOL_TURNS" in
    1)
      printf '%s\n' "$first_prompt"
      ;;
    2)
      printf '%s\n' "$first_prompt" "$second_prompt"
      ;;
    *)
      echo "error: TOOL_TURNS must be 1 or 2, got ${TOOL_TURNS}" >&2
      exit 2
      ;;
  esac
}

run_recording() {
  local label="$1"
  local output_name="$2"
  local transport="$3"
  local backend_flag="$4"
  local backend_url="$5"
  local model="$6"
  local tools_file="$7"
  local first_prompt="$8"
  local second_prompt="$9"

  require_file "$RECORDER"
  require_file "$tools_file"
  require_file "$TOOL_OUTPUTS"
  mkdir -p "$OUT"

  local output_path="${OUT%/}/${output_name}"
  local proxy_port
  proxy_port="$(alloc_proxy_port)"

  echo
  echo "==> ${label}"
  echo "    output: ${output_path}"
  echo "    target: ${backend_url}"
  echo "    model:  ${model}"
  echo "    wire:   ${transport}"

  emit_prompts "$first_prompt" "$second_prompt" |
    "$PYTHON" "$RECORDER" \
      --turns "$TOOL_TURNS" \
      --mode responses \
      --transport "$transport" \
      --stream \
      --proxy-port "$proxy_port" \
      "$backend_flag" "$backend_url" \
      --model "$model" \
      --tools "$tools_file" \
      --tool-outputs "$TOOL_OUTPUTS" \
      --output "$output_path"
}

record_gateway_http_custom() {
  run_recording \
    "gateway HTTP/SSE custom tool" \
    "codex-gateway-http-custom-tool-${GATEWAY_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--vllm" \
    "$GATEWAY_URL" \
    "$GATEWAY_MODEL" \
    "$CUSTOM_TOOL" \
    'You must call the agentic_raw_echo custom tool with exactly CUSTOM_CASSETTE_OK before answering.' \
    'Use the custom tool output. Return only CUSTOM_CASSETTE_OUTPUT_OK.'
}

record_gateway_http() {
  run_recording \
    "gateway HTTP/SSE function tool" \
    "codex-gateway-http-function-tool-${GATEWAY_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--vllm" \
    "$GATEWAY_URL" \
    "$GATEWAY_MODEL" \
    "$FUNCTION_TOOL" \
    'You must call the agentic_plain_echo tool with text exactly "plain fixture" before answering.' \
    'Use the tool output. Return only the echo string.'

  run_recording \
    "gateway HTTP/SSE namespace tool" \
    "codex-gateway-http-namespace-tool-${GATEWAY_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--vllm" \
    "$GATEWAY_URL" \
    "$GATEWAY_MODEL" \
    "$NAMESPACE_TOOL" \
    'You must call mcp__agentic_fixture.add_numbers with numbers [8, 0] before answering.' \
    'Use the tool output. Return only the sum.'

  record_gateway_http_custom
}

record_gateway_ws_custom() {
  run_recording \
    "gateway WebSocket custom tool" \
    "codex-gateway-websocket-custom-tool-${GATEWAY_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--vllm" \
    "$GATEWAY_URL" \
    "$GATEWAY_MODEL" \
    "$CUSTOM_TOOL" \
    'You must call the agentic_raw_echo custom tool with exactly CUSTOM_CASSETTE_OK before answering.' \
    'Use the custom tool output. Return only CUSTOM_CASSETTE_OUTPUT_OK.'
}

record_gateway_ws() {
  run_recording \
    "gateway WebSocket function tool" \
    "codex-gateway-websocket-function-tool-${GATEWAY_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--vllm" \
    "$GATEWAY_URL" \
    "$GATEWAY_MODEL" \
    "$FUNCTION_TOOL" \
    'You must call the agentic_plain_echo tool with text exactly "plain fixture" before answering.' \
    'Use the tool output. Return only the echo string.'

  run_recording \
    "gateway WebSocket namespace tool" \
    "codex-gateway-websocket-namespace-tool-${GATEWAY_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--vllm" \
    "$GATEWAY_URL" \
    "$GATEWAY_MODEL" \
    "$NAMESPACE_TOOL" \
    'You must call mcp__agentic_fixture.add_numbers with numbers [8, 0] before answering.' \
    'Use the tool output. Return only the sum.'

  record_gateway_ws_custom
}

record_direct_vllm_http_custom() {
  require_direct_vllm_url

  run_recording \
    "direct vLLM HTTP/SSE custom tool" \
    "codex-direct-vllm-http-custom-tool-${V_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--vllm" \
    "$VLLM_URL" \
    "$V_MODEL" \
    "$CUSTOM_TOOL" \
    'You must call the agentic_raw_echo custom tool with exactly CUSTOM_CASSETTE_OK before answering.' \
    'Use the custom tool output. Return only CUSTOM_CASSETTE_OUTPUT_OK.'
}

record_direct_vllm_http() {
  require_direct_vllm_url

  run_recording \
    "direct vLLM HTTP/SSE function tool" \
    "codex-direct-vllm-http-function-tool-${V_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--vllm" \
    "$VLLM_URL" \
    "$V_MODEL" \
    "$FUNCTION_TOOL" \
    'You must call the agentic_plain_echo tool with text exactly "plain fixture" before answering.' \
    'Use the tool output. Return only the echo string.'

  run_recording \
    "direct vLLM HTTP/SSE flattened namespace tool" \
    "codex-direct-vllm-http-flat-namespace-tool-${V_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--vllm" \
    "$VLLM_URL" \
    "$V_MODEL" \
    "$DIRECT_VLLM_FLAT_NAMESPACE_TOOL" \
    'You must call agentic_ns__mcp__agentic_fixture__add_numbers with numbers [8, 0] before answering.' \
    'Use the tool output. Return only the sum.'

  record_direct_vllm_http_custom
}

record_direct_vllm_ws() {
  require_direct_vllm_url

  run_recording \
    "direct vLLM WebSocket function tool" \
    "codex-direct-vllm-websocket-function-tool-${V_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--vllm" \
    "$VLLM_URL" \
    "$V_MODEL" \
    "$FUNCTION_TOOL" \
    'You must call the agentic_plain_echo tool with text exactly "plain fixture" before answering.' \
    'Use the tool output. Return only the echo string.'

  run_recording \
    "direct vLLM WebSocket flattened namespace tool" \
    "codex-direct-vllm-websocket-flat-namespace-tool-${V_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--vllm" \
    "$VLLM_URL" \
    "$V_MODEL" \
    "$DIRECT_VLLM_FLAT_NAMESPACE_TOOL" \
    'You must call agentic_ns__mcp__agentic_fixture__add_numbers with numbers [8, 0] before answering.' \
    'Use the tool output. Return only the sum.'
}

record_openai_https() {
  require_openai_key

  run_recording \
    "direct OpenAI HTTPS/SSE function tool" \
    "codex-openai-https-function-tool-${OPENAI_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--openai" \
    "$OPENAI_URL" \
    "$OPENAI_MODEL" \
    "$FUNCTION_TOOL" \
    'You must call the agentic_plain_echo tool with text exactly "plain fixture" before answering.' \
    'Use the tool output. Return only the echo string.'

  record_openai_https_custom
}

record_openai_https_custom() {
  require_openai_key

  run_recording \
    "direct OpenAI HTTPS/SSE custom tool" \
    "codex-openai-https-custom-tool-${OPENAI_CUSTOM_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--openai" \
    "$OPENAI_URL" \
    "$OPENAI_CUSTOM_MODEL" \
    "$CUSTOM_TOOL" \
    'You must call the agentic_raw_echo custom tool with exactly CUSTOM_CASSETTE_OK before answering.' \
    'Use the custom tool output. Return only CUSTOM_CASSETTE_OUTPUT_OK.'
}

record_openai_ws() {
  require_openai_key

  run_recording \
    "direct OpenAI WebSocket function tool" \
    "codex-openai-websocket-function-tool-${OPENAI_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--openai" \
    "$OPENAI_URL" \
    "$OPENAI_MODEL" \
    "$FUNCTION_TOOL" \
    'You must call the agentic_plain_echo tool with text exactly "plain fixture" before answering.' \
    'Use the tool output. Return only the echo string.'

  record_openai_ws_custom
}

record_openai_ws_custom() {
  require_openai_key

  run_recording \
    "direct OpenAI WebSocket custom tool" \
    "codex-openai-websocket-custom-tool-${OPENAI_CUSTOM_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--openai" \
    "$OPENAI_URL" \
    "$OPENAI_CUSTOM_MODEL" \
    "$CUSTOM_TOOL" \
    'You must call the agentic_raw_echo custom tool with exactly CUSTOM_CASSETTE_OK before answering.' \
    'Use the custom tool output. Return only CUSTOM_CASSETTE_OUTPUT_OK.'
}

record_openai_namespace_https() {
  require_openai_key

  run_recording \
    "direct OpenAI HTTPS/SSE raw namespace tool" \
    "codex-openai-https-namespace-tool-${OPENAI_MODEL_SLUG}-streaming.yaml" \
    "http" \
    "--openai" \
    "$OPENAI_URL" \
    "$OPENAI_MODEL" \
    "$NAMESPACE_TOOL" \
    'You must call mcp__agentic_fixture.add_numbers with numbers [8, 0] before answering.' \
    'Use the tool output. Return only the sum.'
}

record_openai_namespace_ws() {
  require_openai_key

  run_recording \
    "direct OpenAI WebSocket raw namespace tool" \
    "codex-openai-websocket-namespace-tool-${OPENAI_MODEL_SLUG}-streaming.yaml" \
    "websocket" \
    "--openai" \
    "$OPENAI_URL" \
    "$OPENAI_MODEL" \
    "$NAMESPACE_TOOL" \
    'You must call mcp__agentic_fixture.add_numbers with numbers [8, 0] before answering.' \
    'Use the tool output. Return only the sum.'
}

case "$TARGET" in
  -h | --help | help)
    usage
    ;;
  all)
    require_openai_key
    require_direct_vllm_url
    record_gateway_http
    record_gateway_ws
    record_direct_vllm_http
    record_openai_https
    record_openai_namespace_https
    record_openai_ws
    record_openai_namespace_ws
    ;;
  gateway)
    record_gateway_http
    record_gateway_ws
    ;;
  gateway-http)
    record_gateway_http
    ;;
  gateway-ws)
    record_gateway_ws
    ;;
  gateway-custom)
    record_gateway_http_custom
    record_gateway_ws_custom
    ;;
  gateway-http-custom)
    record_gateway_http_custom
    ;;
  gateway-ws-custom)
    record_gateway_ws_custom
    ;;
  direct-vllm | direct-vllm-http)
    record_direct_vllm_http
    ;;
  direct-vllm-custom | direct-vllm-http-custom)
    record_direct_vllm_http_custom
    ;;
  direct-vllm-ws)
    record_direct_vllm_ws
    ;;
  openai | openai-https)
    record_openai_https
    ;;
  openai-ws)
    record_openai_ws
    ;;
  openai-custom)
    record_openai_https_custom
    record_openai_ws_custom
    ;;
  openai-https-custom)
    record_openai_https_custom
    ;;
  openai-ws-custom)
    record_openai_ws_custom
    ;;
  openai-namespace)
    record_openai_namespace_https
    ;;
  openai-ws-namespace)
    record_openai_namespace_ws
    ;;
  experimental-all)
    require_openai_key
    require_direct_vllm_url
    record_gateway_http
    record_gateway_ws
    record_direct_vllm_http
    record_direct_vllm_ws
    record_openai_https
    record_openai_ws
    record_openai_namespace_https
    record_openai_namespace_ws
    ;;
  *)
    usage >&2
    echo >&2
    echo "error: unknown target: ${TARGET}" >&2
    exit 2
    ;;
esac
