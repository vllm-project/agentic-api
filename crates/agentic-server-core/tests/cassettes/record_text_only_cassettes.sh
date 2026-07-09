#!/usr/bin/env bash
# record_text_only_cassettes.sh
#
# Records all cassettes (responses + conversation) in sequence.
# The proxy is embedded inside record_cassette.py — no separate proxy needed.
#
# Prerequisites:
#   - OPENAI_API_KEY must be set in the environment
#
# Usage:
#   bash tests/cassettes/record_text_only_cassettes.sh
#   MODEL=gpt-4.1-mini bash tests/cassettes/record_text_only_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/text_only"
RESPONSES_DIR="$BASE_DIR/responses"
CONV_DIR="$BASE_DIR/conversation"
MODEL="${MODEL:-gpt-4o}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

next_test() {
    echo
    read -rp "Press ENTER when ready for the next test..."
    echo
}

mkdir -p "$RESPONSES_DIR" "$CONV_DIR"

# ══════════════════════════════════════════════════════════════════
# RESPONSES (previous_response_id chaining, no conversation object)
# ══════════════════════════════════════════════════════════════════

# ── Test 1: single-turn non-streaming ────────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 1 of 9 — resp-single-nonstreaming"
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
    --output "$RESPONSES_DIR/resp-single-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 1 done."
next_test

# ── Test 2: single-turn streaming ────────────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 2 of 9 — resp-single-streaming"
bold "  1 turn, streaming"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Reply with exactly one word: WORLD"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 1 \
    --model "$MODEL" \
    --output "$RESPONSES_DIR/resp-single-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 2 done."
next_test

# ── Test 3: two-turn non-streaming ───────────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 3 of 9 — resp-two-turn-nonstreaming"
bold "  2 turns, non-streaming, previous_response_id chaining"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Remember the word APPLE. Just say: OK"
echo "  Turn 2: What word did I ask you to remember?"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 2 \
    --no-stream \
    --model "$MODEL" \
    --output "$RESPONSES_DIR/resp-two-turn-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 3 done."
next_test

# ── Test 4: two-turn streaming ────────────────────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 4 of 9 — resp-two-turn-streaming"
bold "  2 turns, streaming, previous_response_id chaining"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Remember the word BANANA. Just say: OK"
echo "  Turn 2: What word did I ask you to remember?"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 2 \
    --model "$MODEL" \
    --output "$RESPONSES_DIR/resp-two-turn-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 4 done."
next_test

# ── Test 5: store=false — follow-up should fail ───────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 5 of 9 — resp-no-store-nonstreaming"
bold "  Turn 1: store=false | Turn 2: previous_response_id → expect error"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Say: NOT STORED"
echo "  Turn 2: follow up"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --turns 2 \
    --no-stream \
    --no-store \
    --model "$MODEL" \
    --output "$RESPONSES_DIR/resp-no-store-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 5 done."
next_test

# ══════════════════════════════════════════════════════════════════
# CONVERSATION (POST /v1/conversations + conversation id chaining)
# ══════════════════════════════════════════════════════════════════

# ── Test 6: 2-turn, non-streaming, conversation ───────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 6 of 9 — conv-two-turn-nonstreaming"
bold "  2 turns, non-streaming, conversation created + chained"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Remember the word CHERRY. Just say: OK"
echo "  Turn 2: What word did I ask you to remember?"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode conv \
    --turns 2 \
    --no-stream \
    --model "$MODEL" \
    --output "$CONV_DIR/conv-two-turn-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 6 done."
next_test

# ── Test 7: 2-turn, streaming, conversation ───────────────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 7 of 9 — conv-two-turn-streaming"
bold "  2 turns, streaming, conversation created + chained"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Remember the word MANGO. Just say: OK"
echo "  Turn 2: What word did I ask you to remember?"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode conv \
    --turns 2 \
    --model "$MODEL" \
    --output "$CONV_DIR/conv-two-turn-${MODEL_SLUG}-streaming.yaml"
green "✓ Test 7 done."
next_test

# ── Test 8: isolation — 2 independent conversations ──────────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 8 of 9 — conv-isolation-nonstreaming"
bold "  2 independent conversations (3 turns each), non-streaming"
bold "  Verifies conversations do not share context"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Conv A | Turn 1: Remember the word ORANGE. Say: OK"
echo "  Conv A | Turn 2: Also remember the word VIOLET. Say: OK"
echo "  Conv A | Turn 3: List every word I asked you to remember, in order, one per line."
echo "  Conv B | Turn 1: Remember the word PURPLE. Say: OK"
echo "  Conv B | Turn 2: Also remember the word INDIGO. Say: OK"
echo "  Conv B | Turn 3: List every word I asked you to remember, in order, one per line."
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode isolation \
    --turns 3 \
    --no-stream \
    --model "$MODEL" \
    --output "$CONV_DIR/conv-isolation-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 8 done."
next_test

── Test 9: branch off turn 1 after 3-turn conversation ──────────

bold "═══════════════════════════════════════════════════════════════"
bold "Test 9 of 9 — conv-branch-nonstreaming (6D)"
bold "  Turns 1-3: conversation chain | Turn 4: branch off turn 1"
bold "  Math: 2+2=4, +1=5, +2=7 | branch: +1 from turn-1 = 5"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: What is 2+2? Reply with just the number."
echo "  Turn 2: Add 1 to your previous answer. Reply with just the number."
echo "  Turn 3: Add 2 to your previous answer. Reply with just the number."
echo "  Branch (off turn 1): Add 1 to your previous answer. Reply with just the number."
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode conv \
    --turns 3 \
    --branch-from 1 \
    --no-stream \
    --model "$MODEL" \
    --output "$CONV_DIR/conv-multi-turn-single-branch-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 9 done."
next_test

# ── Test 10: 5-turn math, branch at turn 1, continue from turn 3 ──

bold "═══════════════════════════════════════════════════════════════"
bold "Test 10 of 10 — conv-branch-turn-number-nonstreaming"
bold "  Turns 1-5: conversation chain | 2 branches"
bold "  Turn1=4, Turn2(from1)=6 | Branch1 turn3(from1)=5, turn4(from3)=8"
bold "  Branch2 turn5(from2)=10"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1 (answer=4): What is 2+2? Reply with just the number."
echo "  Turn 2 (from turn 1, answer=4+2): Add 2 to your previous answer. Reply with just the number."
echo "  Branch 1 | turn 3 (from turn 1, answer=4+1): Add 1. Reply with just the number."
echo "  Branch 1 | turn 4 (from turn 3, answer=5+3): Add 3 to your previous answer. Reply with just the number."
echo "  Branch 2 | turn 5 (from turn 2, answer=6+4): Add 4. Reply with just the number."
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode conv \
    --turns 5 \
    --branch-from 1 \
    --branch-turn-number 3 \
    --branch-from 2 \
    --branch-turn-number 5 \
    --no-stream \
    --model "$MODEL" \
    --output "$CONV_DIR/conv-multi-branch-multi-turn-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 10 done."

next_test

# ── Test 11: store=false follow-up with conversation_id still rehydrates + persists ──

bold "═══════════════════════════════════════════════════════════════"
bold "Test 11 of 11 — conv-store-false-followup-nonstreaming"
bold "  Turn 1: store=true + conversation_id (stored)"
bold "  Turn 2: store=false + same conversation_id (rehydrates, persists)"
bold "═══════════════════════════════════════════════════════════════"
bold "Prompts to enter:"
echo "  Turn 1: Remember the word LEMON. Just say: OK"
echo "  Turn 2: What word did I ask you to remember?"
echo
python "$SCRIPTS_DIR/record_cassette.py" \
    --mode store_true_then_store_false \
    --turns 2 \
    --no-stream \
    --model "$MODEL" \
    --output "$CONV_DIR/conv-store-false-followup-${MODEL_SLUG}-nonstreaming.yaml"
green "✓ Test 11 done."

echo
green "════════════════════════════════════════════════════════════════"
green "All 11 cassettes recorded."
green "════════════════════════════════════════════════════════════════"
