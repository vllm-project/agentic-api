#!/usr/bin/env bash
# record_reasoning_cassettes.sh
#
# Records reasoning cassettes from a vLLM server (two-turn, streaming and non-streaming).
# Uses --mode responses with --vllm, which is the only supported combination.
#
# Prerequisites:
#   - vLLM server must be running and accessible at VLLM_URL
#
# Usage:
#   bash tests/cassettes/record_reasoning_cassettes.sh
#   VLLM_URL=http://localhost:8000 MODEL=Qwen/Qwen3-30B-A3B-FP8 bash tests/cassettes/record_reasoning_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/reasoning"
RESPONSES_DIR="$BASE_DIR/responses"
VLLM_URL="${VLLM_URL:-http://localhost:8000}"
MODEL="${MODEL:-Qwen/Qwen3-30B-A3B-FP8}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

next_test() {
    echo
    read -rp "Press ENTER when ready for the next test..."
    echo
}

mkdir -p "$RESPONSES_DIR"

bold "vLLM URL: $VLLM_URL"
bold "Model:    $MODEL"
echo

# ── Test 1: single-turn non-streaming ────────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 1 of 2 — reasoning-single-nonstreaming"
bold "  1 turn, non-streaming"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Reply with exactly one word: HELLO"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --no-stream \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --output "$RESPONSES_DIR/reasoning-single-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 1 done."
next_test

# ── Test 2: single-turn streaming ────────────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 2 of 2 — reasoning-single-streaming"
bold "  1 turn, streaming"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Reply with exactly one word: HELLO"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --output "$RESPONSES_DIR/reasoning-single-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 2 done."

echo
green "════════════════════════════════════════════════════════════════"
green "All 2 reasoning cassettes recorded -> $RESPONSES_DIR"
green "════════════════════════════════════════════════════════════════"
