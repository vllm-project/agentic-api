#!/usr/bin/env bash
# Record real SGLang traffic using the shared recorder and Dynamo scenario definitions.
# See docs/guides/sglang-upstream.md for the pinned launch configuration.

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST_DIR="$SCRIPTS_DIR/sglang"
STAGING_ROOT="$(mktemp -d -t agentic-sglang-recording.XXXXXXXX)"
BASE_DIR="$STAGING_ROOT/sglang"
TOOLS_FILE="$SCRIPTS_DIR/tool_calls/tools.json"
SGLANG_URL="${SGLANG_URL:-http://127.0.0.1:30000}"
MODEL="${MODEL:-Qwen/Qwen3-8B}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"
PYTHON="${PYTHON:-python}"
SGLANG_VERSION="${SGLANG_VERSION:-0.5.18}"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

mkdir -p "$BASE_DIR"

bold "SGLang URL: $SGLANG_URL"
bold "Model:      $MODEL"
echo

# record NAME STREAM_FLAG PROMPT [extra recorder args...]
record() {
    local name="$1" stream_flag="$2" prompt="$3"; shift 3
    local suffix; [[ -n "$stream_flag" ]] && suffix=nonstreaming || suffix=streaming
    bold "── $name ($suffix) ──"
    record_into "$BASE_DIR/${name}-${MODEL_SLUG}-${suffix}.yaml" "$stream_flag" "$prompt" "$@"
    green "✓ $name ($suffix) done."
}

# append_turn NAME STREAM_FLAG PROMPT [extra recorder args...]
# The recorder truncates its output file, so extra turns are recorded separately and merged.
append_turn() {
    local name="$1" stream_flag="$2" prompt="$3"; shift 3
    local suffix; [[ -n "$stream_flag" ]] && suffix=nonstreaming || suffix=streaming
    local target="$BASE_DIR/${name}-${MODEL_SLUG}-${suffix}.yaml"
    local extra="${target%.yaml}.next-turn.yaml"
    bold "── $name ($suffix), next turn ──"
    record_into "$extra" "$stream_flag" "$prompt" "$@"
    $PYTHON - "$target" "$extra" <<'PY'
import sys, yaml
target, extra = sys.argv[1], sys.argv[2]
merged = yaml.safe_load(open(target))
for turn in yaml.safe_load(open(extra))["turns"]:
    turn["filename"] = f"t{len(merged['turns']) + 1}"
    merged["turns"].append(turn)
yaml.safe_dump(merged, open(target, "w"), sort_keys=False, allow_unicode=True, width=10**9)
PY
    rm -f "$extra"
    green "✓ $name ($suffix) turn appended."
}

record_into() {
    local output="$1" stream_flag="$2" prompt="$3"; shift 3
    # shellcheck disable=SC2086
    printf '%s\n' "$prompt" | $PYTHON "$SCRIPTS_DIR/record_cassette.py" \
        --mode responses \
        --turns 1 \
        --model "$MODEL" \
        --vllm "$SGLANG_URL" \
        --max-output-tokens 2048 \
        $stream_flag \
        "$@" \
        --output "$output"
}

# hydrated_turn2_input CASSETTE OUT_JSON
# Writes the item history the gateway sends for turn 2: the user prompt, the assistant message exactly as
# recorded in turn 1 (same id and text), and the follow-up user prompt.
hydrated_turn2_input() {
    $PYTHON - "$1" "$2" "$TURN1_PROMPT" "$TURN2_PROMPT" <<'PY'
import json, sys, yaml
cassette, out, turn1, turn2 = sys.argv[1:5]
response = yaml.safe_load(open(cassette))["turns"][0]["response"]
if response.get("body"):
    completed = response["body"]
else:
    completed = next(
        json.loads(line[len("data: "):])["response"]
        for raw in response["sse"]
        for line in raw.splitlines()
        if line.startswith("data: ") and json.loads(line[len("data: "):]).get("type") == "response.completed"
    )
history = [{"type": "message", "role": "user", "content": turn1}]
for item in completed["output"]:
    if item["type"] == "reasoning":
        history.append(item)
    elif item["type"] == "message":
        history.append({
        "type": "message",
        "id": item["id"],
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": part["text"]} for part in item["content"]],
        })
    else:
        raise ValueError(f"Unexpected output item in text scenario: {item['type']}")
history.append({"type": "message", "role": "user", "content": turn2})
json.dump(history, open(out, "w"), indent=2)
PY
}

TURN1_PROMPT="Remember the word APPLE. Just say: OK"
TURN2_PROMPT="What word did I ask you to remember? Reply with just the word."

for stream_flag in --no-stream ""; do
    [[ -n "$stream_flag" ]] && suffix=nonstreaming || suffix=streaming
    record sglang-stateful "$stream_flag" "$TURN1_PROMPT"
    turn2_input="$(mktemp --suffix=.json)"
    hydrated_turn2_input "$BASE_DIR/sglang-stateful-${MODEL_SLUG}-${suffix}.yaml" "$turn2_input"
    append_turn sglang-stateful "$stream_flag" "" --input-file "$turn2_input"
    rm -f "$turn2_input"
    record sglang-tool-call-auto "$stream_flag" "What is the current NVIDIA stock price? Use the tool." \
        --tools "$TOOLS_FILE" --tool-choice auto
done

echo
"$PYTHON" "$SCRIPTS_DIR/prepare_sglang_recordings.py" "$BASE_DIR" "$SGLANG_VERSION" "$MODEL" "$SGLANG_URL"
"$PYTHON" "$SCRIPTS_DIR/../../../../scripts/validate-cassettes.py" "$BASE_DIR"
(
    cd "$SCRIPTS_DIR/../../../.."
    PROVIDER_CASSETTE_ROOT="$STAGING_ROOT" cargo test -p agentic-server-core --test sglang_cassette_test
)
mkdir -p "$DEST_DIR"
cp "$BASE_DIR"/*.yaml "$DEST_DIR/"
green "All SGLang cassettes validated and published -> $DEST_DIR (staging: $STAGING_ROOT)"
