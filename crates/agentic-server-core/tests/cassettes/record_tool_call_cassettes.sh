#!/usr/bin/env bash
# record_tool_call_cassettes.sh
#
# Records tool-call cassettes for all four tool_choice modes (streaming + non-streaming):
#   auto, none, required, named
#
# Prerequisites:
#   - vLLM server running at VLLM_URL with tool-call support:
#       vllm serve <model> --tool-call-parser hermes --enable-auto-tool-choice --port 5050
#
# Usage:
#   bash tests/cassettes/record_tool_call_cassettes.sh
#   VLLM_URL=http://localhost:5050 MODEL=Qwen/Qwen3-30B-A3B-FP8 bash tests/cassettes/record_tool_call_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/tool_calls"
TOOLS_FILE="$BASE_DIR/tools.json"
VLLM_URL="${VLLM_URL:-http://localhost:5050}"
MODEL="${MODEL:-Qwen/Qwen3-30B-A3B-FP8}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

next_test() {
    echo
    read -rp "Press ENTER when ready for the next test..."
    echo
}

mkdir -p "$BASE_DIR"

bold "vLLM URL: $VLLM_URL"
bold "Model:    $MODEL"
echo

# ── Test 1: tool_choice=auto, non-streaming ───────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 1 of 8 — tool-call-auto-nonstreaming"
bold "  tool_choice=auto — model picks get_stock_price + search_web"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  What is the current NVIDIA stock price, and search the web for the latest vLLM release news?"
echo
printf 'What is the current NVIDIA stock price, and search the web for the latest vLLM release news?\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --no-stream \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice "auto" \
    --output "$BASE_DIR/tool-call-auto-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 1 done."
next_test

# ── Test 2: tool_choice=auto, streaming ──────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 2 of 8 — tool-call-auto-streaming"
bold "  tool_choice=auto — model picks get_stock_price + search_web"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  What is the current NVIDIA stock price, and search the web for the latest vLLM release news?"
echo
printf 'What is the current NVIDIA stock price, and search the web for the latest vLLM release news?\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice "auto" \
    --output "$BASE_DIR/tool-call-auto-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 2 done."
next_test

# ── Test 3: tool_choice=none, non-streaming ───────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 3 of 8 — tool-call-none-nonstreaming"
bold "  tool_choice=none — tool calling blocked, plain text response"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  Translate the phrase Hello, how are you? into Japanese."
echo
printf 'Translate the phrase Hello, how are you? into Japanese.\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --no-stream \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice "none" \
    --output "$BASE_DIR/tool-call-none-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 3 done."
next_test

# ── Test 4: tool_choice=none, streaming ──────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 4 of 8 — tool-call-none-streaming"
bold "  tool_choice=none — tool calling blocked, plain text response"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  Translate the phrase Hello, how are you? into Japanese."
echo
printf 'Translate the phrase Hello, how are you? into Japanese.\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice "none" \
    --output "$BASE_DIR/tool-call-none-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 4 done."
next_test

# ── Test 5: tool_choice=required, non-streaming ───────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 5 of 8 — tool-call-required-nonstreaming"
bold "  tool_choice=required — model must call tools (calculate + send_email)"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  Calculate (128 * 0.75) + 42, and send an email to alice@example.com with subject Daily Report and body All systems nominal."
echo
printf 'Calculate (128 * 0.75) + 42, and send an email to alice@example.com with subject Daily Report and body All systems nominal.\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --no-stream \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice "required" \
    --output "$BASE_DIR/tool-call-required-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 5 done."
next_test

# ── Test 6: tool_choice=required, streaming ──────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 6 of 8 — tool-call-required-streaming"
bold "  tool_choice=required — model must call tools (calculate + send_email)"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  Calculate (128 * 0.75) + 42, and send an email to alice@example.com with subject Daily Report and body All systems nominal."
echo
printf 'Calculate (128 * 0.75) + 42, and send an email to alice@example.com with subject Daily Report and body All systems nominal.\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice "required" \
    --output "$BASE_DIR/tool-call-required-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 6 done."
next_test

# ── Test 7: tool_choice=named (translate_text), non-streaming ─────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 7 of 8 — tool-call-named-nonstreaming"
bold "  tool_choice={type:function,name:translate_text} — forces translate_text"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  Translate Good morning, have a great day! into Spanish."
echo
printf 'Translate Good morning, have a great day! into Spanish.\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --no-stream \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice '{"type":"function","name":"translate_text"}' \
    --output "$BASE_DIR/tool-call-named-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 7 done."
next_test

# ── Test 8: tool_choice=named (translate_text), streaming ─────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 8 of 8 — tool-call-named-streaming"
bold "  tool_choice={type:function,name:translate_text} — forces translate_text"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompt:"
echo "  Translate Good morning, have a great day! into Spanish."
echo
printf 'Translate Good morning, have a great day! into Spanish.\n' \
| python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --model "$MODEL" \
    --vllm "$VLLM_URL" \
    --tools "$TOOLS_FILE" \
    --tool-choice '{"type":"function","name":"translate_text"}' \
    --output "$BASE_DIR/tool-call-named-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 8 done."

echo
green "════════════════════════════════════════════════════════════════"
green "All 8 tool-call cassettes recorded -> $BASE_DIR"
green "════════════════════════════════════════════════════════════════"
