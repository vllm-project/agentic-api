#!/usr/bin/env bash
# record_messages_cassettes.sh
#
# Records Anthropic Messages (/v1/messages) gateway-tool cassettes: a two-turn
# session where the model emits a gateway-owned `web_search` tool_use (turn 1),
# the tool_result is fed back, and the model produces the final answer (turn 2).
# Recorded both non-streaming and streaming.
#
# These fixtures capture the upstream Messages traffic that the Claude Code
# server-side gateway tool loop (issue #115) will replay in tests.
#
# Prerequisites:
#   - vLLM (>= 0.25.1, which serves /v1/messages natively) running at VLLM_URL
#     with tool-call support:
#       vllm serve <model> --tool-call-parser hermes --enable-auto-tool-choice \
#         --reasoning-parser qwen3 --served-model-name <model>
#
# Usage:
#   bash tests/cassettes/record_messages_cassettes.sh
#   VLLM_URL=http://localhost:8200 MODEL=qwen3 bash tests/cassettes/record_messages_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/messages"
TOOLS_FILE="$BASE_DIR/tools.json"
TOOL_OUTPUTS_FILE="$BASE_DIR/tool_outputs.json"
VLLM_URL="${VLLM_URL:-http://localhost:8200}"
MODEL="${MODEL:-qwen3}"
MODEL_SLUG="Qwen-Qwen3-30B-A3B-FP8"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

PROMPT='What is the latest stable Rust release? Use web_search.'

bold "Recording Messages web_search cassette (non-streaming)"
printf '%s\n' "$PROMPT" | python "$SCRIPTS_DIR/record_cassette.py" \
    --mode messages \
    --turns 2 \
    --no-stream \
    --vllm "$VLLM_URL" \
    --model "$MODEL" \
    --tools "$TOOLS_FILE" \
    --tool-outputs "$TOOL_OUTPUTS_FILE" \
    --max-output-tokens 1024 \
    --output "$BASE_DIR/messages-web-search-${MODEL_SLUG}-nonstreaming.yaml"

bold "Recording Messages web_search cassette (streaming)"
printf '%s\n' "$PROMPT" | python "$SCRIPTS_DIR/record_cassette.py" \
    --mode messages \
    --turns 2 \
    --stream \
    --vllm "$VLLM_URL" \
    --model "$MODEL" \
    --tools "$TOOLS_FILE" \
    --tool-outputs "$TOOL_OUTPUTS_FILE" \
    --max-output-tokens 1024 \
    --output "$BASE_DIR/messages-web-search-${MODEL_SLUG}-streaming.yaml"

green "Done. Cassettes written to $BASE_DIR/"
