#!/usr/bin/env bash
# Records the same two-turn image-input conversation against OpenAI and the
# gateway. Turn 1 sends the committed PNG fixture inline as an `input_image`
# data URL next to an `input_text` part; turn 2 continues that response by
# `previous_response_id` with a text-only follow-up.
#
# Each provider records one streaming and one non-streaming cassette. The
# default `all` set records the OpenAI ground truth and its gateway counterpart.
#
# Usage from the repository root:
#   OPENAI_API_KEY=sk-... \
#     bash crates/agentic-server-core/tests/cassettes/record_image_input_cassettes.sh
#   IMAGE_RECORD_SET=gateway GATEWAY_URL=http://localhost:9000 \
#     bash crates/agentic-server-core/tests/cassettes/record_image_input_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="${IMAGE_OUTPUT_DIR:-$SCRIPTS_DIR/images/responses}"
INPUT_DIR="$SCRIPTS_DIR/images/inputs"
IMAGE_FILE="$INPUT_DIR/red-blue-64.png"
INPUT_FILE="$INPUT_DIR/image-turn.json"
RECORDER="${RECORDER:-$SCRIPTS_DIR/record_cassette.py}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
MODEL="${MODEL:-gpt-4o}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4o}"
OPENAI_MODEL_SLUG="$(echo "$OPENAI_MODEL" | tr '/: ' '---')"
IMAGE_RECORD_SET="${IMAGE_RECORD_SET:-all}"
FOLLOW_UP_PROMPT='Without repeating the colors, reply with exactly one word: did my previous message include an image? YES or NO.'
STAGING_DIR=""
STAGED_OUTPUTS=()
FINAL_OUTPUTS=()

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }

cleanup_staging() {
  if [[ -n "$STAGING_DIR" ]]; then
    rm -rf -- "$STAGING_DIR"
  fi
}

trap cleanup_staging EXIT

# The committed JSON turn must embed exactly the committed PNG, so a recording
# can never drift from the fixture the replay tests compare against.
validate_input_fixture() {
  python - "$IMAGE_FILE" "$INPUT_FILE" <<'PY'
import base64
import json
import sys
from pathlib import Path

image = Path(sys.argv[1]).read_bytes()
turn = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
expected_url = "data:image/png;base64," + base64.b64encode(image).decode("ascii")
parts = turn[0]["content"]
images = [part for part in parts if part.get("type") == "input_image"]
if len(images) != 1 or images[0].get("image_url") != expected_url:
    raise SystemExit("ERROR: image-turn.json does not embed red-blue-64.png as its single input_image part")
if not any(part.get("type") == "input_text" for part in parts):
    raise SystemExit("ERROR: image-turn.json must pair the image with an input_text part")
PY
}

validate_recorded_conversation() {
  local file="$1"
  local stream_flag="$2"

  python - "$file" "$stream_flag" "$INPUT_FILE" "$FOLLOW_UP_PROMPT" <<'PY'
import json
import sys
from pathlib import Path

import yaml

path = Path(sys.argv[1])
streaming = sys.argv[2] == "--stream"
expected_input = json.loads(Path(sys.argv[3]).read_text(encoding="utf-8"))
follow_up = sys.argv[4]
document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
turns = document.get("turns") or []
if len(turns) != 2:
    raise SystemExit(f"ERROR: expected two recorded turns in {path}, found {len(turns)}")


def terminal_response(turn: dict) -> dict:
    response = turn.get("response") or {}
    status_code = response.get("status_code")
    if status_code != 200:
        raise SystemExit(f"ERROR: recording returned HTTP {status_code}: {response.get('body')}")
    if not streaming:
        return response.get("body") or {}
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
    return next(
        (event.get("response") for event in reversed(events) if event.get("type") == "response.completed"),
        None,
    ) or {}


def check_completed_message(terminal: dict, label: str) -> None:
    if terminal.get("status") != "completed":
        raise SystemExit(f"ERROR: {label} did not complete: {terminal}")
    output = terminal.get("output") or []
    messages = [item for item in output if item.get("type") == "message"]
    if not messages:
        raise SystemExit(f"ERROR: {label} has no message item: {[item.get('type') for item in output]}")
    text = "".join(
        part.get("text", "")
        for item in messages
        for part in item.get("content") or []
        if part.get("type") == "output_text"
    )
    if not text.strip():
        raise SystemExit(f"ERROR: {label} produced no output_text")


first, second = turns
first_request = (first.get("request") or {}).get("body") or {}
if first_request.get("input") != expected_input:
    raise SystemExit("ERROR: turn 1 request input differs from image-turn.json")
if first_request.get("stream") is not streaming:
    raise SystemExit(f"ERROR: turn 1 stream mode differs from {streaming}")
first_terminal = terminal_response(first)
check_completed_message(first_terminal, "turn 1")

second_request = (second.get("request") or {}).get("body") or {}
if second_request.get("input") != follow_up:
    raise SystemExit(f"ERROR: turn 2 request input is not the follow-up prompt: {second_request.get('input')!r}")
if second_request.get("previous_response_id") != first_terminal.get("id"):
    raise SystemExit(
        "ERROR: turn 2 previous_response_id does not reference turn 1: "
        f"{second_request.get('previous_response_id')!r} != {first_terminal.get('id')!r}"
    )
if second_request.get("stream") is not streaming:
    raise SystemExit(f"ERROR: turn 2 stream mode differs from {streaming}")
check_completed_message(terminal_response(second), "turn 2")
PY
}

record_conversation() {
  local endpoint_flag="$1"
  local endpoint="$2"
  local model="$3"
  local output="$4"
  local stream_flag="$5"
  local staged_output
  local temporary_output

  staged_output="$STAGING_DIR/$(basename "$output")"
  temporary_output="$(mktemp "$STAGING_DIR/.image-cassette.XXXXXX")"

  if ! printf '%s\n' "$FOLLOW_UP_PROMPT" \
    | python "$RECORDER" \
        --mode responses \
        --turns 2 \
        "$stream_flag" \
        --model "$model" \
        "$endpoint_flag" "$endpoint" \
        --input-file "$INPUT_FILE" \
        --max-output-tokens 64 \
        --output "$temporary_output"
  then
    rm -f -- "$temporary_output"
    return 1
  fi

  if ! validate_recorded_conversation "$temporary_output" "$stream_flag"; then
    rm -f -- "$temporary_output"
    return 1
  fi
  mv -- "$temporary_output" "$staged_output"
  STAGED_OUTPUTS+=("$staged_output")
  FINAL_OUTPUTS+=("$output")
  green "✓ image-input cassette validated -> $output"
}

promote_recorded_suite() {
  local index

  for index in "${!STAGED_OUTPUTS[@]}"; do
    mv -- "${STAGED_OUTPUTS[$index]}" "${FINAL_OUTPUTS[$index]}"
    green "✓ image-input cassette promoted -> ${FINAL_OUTPUTS[$index]}"
  done
}

record_provider_suite() {
  local provider="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local output_prefix="$5"

  bold "$provider image-input cassettes"
  bold "Endpoint: $endpoint"
  bold "Model:    $model"
  bold "Image:    $IMAGE_FILE"

  bold "$provider streaming image-input conversation"
  record_conversation \
    "$endpoint_flag" "$endpoint" "$model" \
    "$BASE_DIR/${output_prefix}-streaming.yaml" \
    --stream

  bold "$provider non-streaming image-input conversation"
  record_conversation \
    "$endpoint_flag" "$endpoint" "$model" \
    "$BASE_DIR/${output_prefix}-nonstreaming.yaml" \
    --no-stream
}

case "$IMAGE_RECORD_SET" in
  gateway|openai|all) ;;
  *)
    echo "ERROR: IMAGE_RECORD_SET must be gateway, openai, or all" >&2
    exit 1
    ;;
esac

validate_input_fixture

# Validate OpenAI requirements before making any live requests. Final fixtures
# remain unchanged until every selected recording has completed validation.
if [[ "$IMAGE_RECORD_SET" == "openai" || "$IMAGE_RECORD_SET" == "all" ]]; then
  if [[ -z "${OPENAI_API_KEY:-}" ]]; then
    echo "ERROR: OPENAI_API_KEY must be set for IMAGE_RECORD_SET=$IMAGE_RECORD_SET" >&2
    exit 1
  fi
fi

mkdir -p "$BASE_DIR"
STAGING_DIR="$(mktemp -d "$BASE_DIR/.image-suite.XXXXXX")"

if [[ "$IMAGE_RECORD_SET" == "openai" || "$IMAGE_RECORD_SET" == "all" ]]; then
  record_provider_suite \
    OpenAI \
    --openai https://api.openai.com \
    "$OPENAI_MODEL" \
    "image-input-openai-reference-${OPENAI_MODEL_SLUG}"
fi

if [[ "$IMAGE_RECORD_SET" == "gateway" || "$IMAGE_RECORD_SET" == "all" ]]; then
  record_provider_suite \
    Gateway \
    --gateway "$GATEWAY_URL" \
    "$MODEL" \
    "image-input-gateway-${MODEL_SLUG}"
fi

promote_recorded_suite
