#!/usr/bin/env bash
# Records paired image-input conversations against OpenAI (the reference path,
# client -> OpenAI Responses API) and the gateway (client -> Agentic API ->
# vLLM hosting an open-source vision model). Both paths receive the same image
# bytes, prompts, and tool definitions; only the model name differs.
#
# Scenarios, each recorded streaming and non-streaming per provider:
#   single-image   one user message with text and an inline PNG
#   multi-image    text and two different PNGs interleaved, order preserved
#   continuation   the single-image turn, then a text follow-up chained by
#                  previous_response_id
#   tool-image     the model calls the client-executed `view_image` function,
#                  the client submits a function_call_output whose output is a
#                  content array carrying the PNG, and the model answers
#
# Usage from the repository root (see README.md "Image input" for the vLLM and
# gateway launch commands):
#   OPENAI_API_KEY=sk-... GATEWAY_URL=http://localhost:9000 MODEL=Qwen/Qwen2.5-VL-3B-Instruct \
#     bash crates/agentic-server-core/tests/cassettes/record_image_input_cassettes.sh
#   IMAGE_RECORD_SET=gateway IMAGE_SCENARIOS=tool-image ... (one provider, one scenario)

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="${IMAGE_OUTPUT_DIR:-$SCRIPTS_DIR/images/responses}"
INPUT_DIR="$SCRIPTS_DIR/images/inputs"
RECORDER="${RECORDER:-$SCRIPTS_DIR/record_cassette.py}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
MODEL="${MODEL:-Qwen/Qwen2.5-VL-3B-Instruct}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4o}"
OPENAI_MODEL_SLUG="$(echo "$OPENAI_MODEL" | tr '/: ' '---')"
IMAGE_RECORD_SET="${IMAGE_RECORD_SET:-all}"
IMAGE_SCENARIOS="${IMAGE_SCENARIOS:-single-image multi-image continuation tool-image}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-64}"
FOLLOW_UP_PROMPT='Without repeating the colors, reply with exactly one word: did my previous message include an image? YES or NO.'
TOOL_PROMPT='Call the view_image tool exactly once, with path "diagram.png". After you see the image, reply with exactly two words: the color on its left half, then the color on its right half.'
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

# The committed JSON turns must embed exactly the committed PNGs, so a
# recording can never drift from the fixtures the replay tests compare against.
validate_input_fixtures() {
  python - "$INPUT_DIR" <<'PY'
import base64
import json
import sys
from pathlib import Path

inputs = Path(sys.argv[1])


def data_url(name: str) -> str:
    return "data:image/png;base64," + base64.b64encode((inputs / name).read_bytes()).decode("ascii")


def image_urls(turn_file: str) -> list[str]:
    turn = json.loads((inputs / turn_file).read_text(encoding="utf-8"))
    return [part["image_url"] for part in turn[0]["content"] if part.get("type") == "input_image"]


if image_urls("single-image.json") != [data_url("red-blue-64.png")]:
    raise SystemExit("ERROR: single-image.json does not embed red-blue-64.png as its single input_image part")
if image_urls("multi-image.json") != [data_url("red-blue-64.png"), data_url("green-yellow-64.png")]:
    raise SystemExit("ERROR: multi-image.json must embed red-blue-64.png then green-yellow-64.png")
tools = json.loads((inputs / "view_image_tool.json").read_text(encoding="utf-8"))
if [tool.get("name") for tool in tools] != ["view_image"]:
    raise SystemExit("ERROR: view_image_tool.json must declare exactly the view_image function")
PY
}

validate_recorded_scenario() {
  local scenario="$1"
  local file="$2"
  local stream_flag="$3"

  python - "$scenario" "$file" "$stream_flag" "$INPUT_DIR" "$FOLLOW_UP_PROMPT" "$TOOL_PROMPT" <<'PY'
import base64
import json
import sys
from pathlib import Path

import yaml

scenario = sys.argv[1]
path = Path(sys.argv[2])
streaming = sys.argv[3] == "--stream"
inputs = Path(sys.argv[4])
follow_up = sys.argv[5]
tool_prompt = sys.argv[6]
document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
turns = document.get("turns") or []
expected_turns = 2 if scenario in {"continuation", "tool-image"} else 1
if len(turns) != expected_turns:
    raise SystemExit(f"ERROR: expected {expected_turns} recorded turn(s) in {path}, found {len(turns)}")


def fixture(name: str):
    return json.loads((inputs / name).read_text(encoding="utf-8"))


def request_body(turn: dict) -> dict:
    return (turn.get("request") or {}).get("body") or {}


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


def check_completed(terminal: dict, label: str) -> None:
    if terminal.get("status") != "completed":
        raise SystemExit(f"ERROR: {label} did not complete: {terminal}")


def check_message(terminal: dict, label: str) -> None:
    check_completed(terminal, label)
    output = terminal.get("output") or []
    text = "".join(
        part.get("text", "")
        for item in output
        if item.get("type") == "message"
        for part in item.get("content") or []
        if part.get("type") == "output_text"
    )
    if not text.strip():
        raise SystemExit(f"ERROR: {label} produced no message text: {[item.get('type') for item in output]}")


for index, turn in enumerate(turns, start=1):
    if request_body(turn).get("stream") is not streaming:
        raise SystemExit(f"ERROR: turn {index} stream mode differs from {streaming}")

first = turns[0]
first_terminal = terminal_response(first)
calls: list[dict] = []

if scenario in {"single-image", "continuation"}:
    if request_body(first).get("input") != fixture("single-image.json"):
        raise SystemExit("ERROR: turn 1 request input differs from single-image.json")
    check_message(first_terminal, "turn 1")
elif scenario == "multi-image":
    if request_body(first).get("input") != fixture("multi-image.json"):
        raise SystemExit("ERROR: turn 1 request input differs from multi-image.json")
    check_message(first_terminal, "turn 1")
elif scenario == "tool-image":
    if request_body(first).get("input") != tool_prompt:
        raise SystemExit("ERROR: turn 1 request input is not the tool prompt")
    if request_body(first).get("tools") != fixture("view_image_tool.json"):
        raise SystemExit("ERROR: turn 1 tools differ from view_image_tool.json")
    check_completed(first_terminal, "turn 1")
    calls = [item for item in first_terminal.get("output") or [] if item.get("type") == "function_call"]
    if [call.get("name") for call in calls] != ["view_image"]:
        raise SystemExit(
            "ERROR: turn 1 must end with exactly one view_image function_call; "
            f"got {[item.get('type') for item in first_terminal.get('output') or []]} -- re-run the recording"
        )
else:
    raise SystemExit(f"ERROR: unknown scenario {scenario}")

if expected_turns == 2:
    second = turns[1]
    second_request = request_body(second)
    if second_request.get("previous_response_id") != first_terminal.get("id"):
        raise SystemExit(
            "ERROR: turn 2 previous_response_id does not reference turn 1: "
            f"{second_request.get('previous_response_id')!r} != {first_terminal.get('id')!r}"
        )
    if scenario == "continuation":
        if second_request.get("input") != follow_up:
            raise SystemExit(f"ERROR: turn 2 request input is not the follow-up prompt: {second_request.get('input')!r}")
    else:
        call = calls[0]
        expected_url = "data:image/png;base64," + base64.b64encode((inputs / "red-blue-64.png").read_bytes()).decode("ascii")
        items = second_request.get("input")
        if not isinstance(items, list) or len(items) != 1 or items[0].get("type") != "function_call_output":
            raise SystemExit(f"ERROR: turn 2 must submit exactly one function_call_output: {items!r}"[:400])
        output_item = items[0]
        if output_item.get("call_id") != call.get("call_id"):
            raise SystemExit("ERROR: turn 2 function_call_output does not answer the recorded call_id")
        parts = output_item.get("output")
        if not isinstance(parts, list) or [part.get("type") for part in parts] != ["input_text", "input_image"]:
            raise SystemExit(f"ERROR: turn 2 output must be an [input_text, input_image] array: {parts!r}"[:400])
        if parts[1].get("image_url") != expected_url:
            raise SystemExit("ERROR: turn 2 tool output does not carry red-blue-64.png")
        if second_request.get("tools") != fixture("view_image_tool.json"):
            raise SystemExit("ERROR: turn 2 tools differ from view_image_tool.json")
    check_message(terminal_response(second), "turn 2")
PY
}

record_scenario() {
  local scenario="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local output="$5"
  local stream_flag="$6"
  local staged_output
  local temporary_output
  local -a recorder_args
  local stdin_lines

  staged_output="$STAGING_DIR/$(basename "$output")"
  temporary_output="$(mktemp "$STAGING_DIR/.image-cassette.XXXXXX")"

  recorder_args=(
    --mode responses
    "$stream_flag"
    --model "$model"
    "$endpoint_flag" "$endpoint"
    --max-output-tokens "$MAX_OUTPUT_TOKENS"
    --output "$temporary_output"
  )
  case "$scenario" in
    single-image)
      recorder_args+=(--turns 1 --input-file "$INPUT_DIR/single-image.json")
      stdin_lines=''
      ;;
    multi-image)
      recorder_args+=(--turns 1 --input-file "$INPUT_DIR/multi-image.json")
      stdin_lines=''
      ;;
    continuation)
      recorder_args+=(--turns 2 --input-file "$INPUT_DIR/single-image.json")
      stdin_lines="$FOLLOW_UP_PROMPT"$'\n'
      ;;
    tool-image)
      recorder_args+=(
        --turns 2
        --tools "$INPUT_DIR/view_image_tool.json"
        --tool-choice auto
        --tool-outputs "$INPUT_DIR/view_image_outputs.py"
      )
      # Turn 1 is the tool prompt; the empty second line makes turn 2 a
      # tool-output-only turn with no user message.
      stdin_lines="$TOOL_PROMPT"$'\n\n'
      ;;
    *)
      echo "ERROR: unknown scenario $scenario" >&2
      return 1
      ;;
  esac

  if ! printf '%s' "$stdin_lines" | python "$RECORDER" "${recorder_args[@]}"; then
    rm -f -- "$temporary_output"
    return 1
  fi

  if ! validate_recorded_scenario "$scenario" "$temporary_output" "$stream_flag"; then
    rm -f -- "$temporary_output"
    return 1
  fi
  mv -- "$temporary_output" "$staged_output"
  STAGED_OUTPUTS+=("$staged_output")
  FINAL_OUTPUTS+=("$output")
  green "✓ $scenario cassette validated -> $output"
}

promote_recorded_suite() {
  local index

  for index in "${!STAGED_OUTPUTS[@]}"; do
    mv -- "${STAGED_OUTPUTS[$index]}" "${FINAL_OUTPUTS[$index]}"
    green "✓ image cassette promoted -> ${FINAL_OUTPUTS[$index]}"
  done
}

record_provider_suite() {
  local provider="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local model_slug="$5"
  local scenario

  bold "$provider image-input cassettes"
  bold "Endpoint:  $endpoint"
  bold "Model:     $model"
  bold "Scenarios: $IMAGE_SCENARIOS"

  for scenario in $IMAGE_SCENARIOS; do
    bold "$provider $scenario (streaming)"
    record_scenario "$scenario" "$endpoint_flag" "$endpoint" "$model" \
      "$BASE_DIR/image-${scenario}-${provider,,}-${model_slug}-streaming.yaml" --stream
    bold "$provider $scenario (non-streaming)"
    record_scenario "$scenario" "$endpoint_flag" "$endpoint" "$model" \
      "$BASE_DIR/image-${scenario}-${provider,,}-${model_slug}-nonstreaming.yaml" --no-stream
  done
}

case "$IMAGE_RECORD_SET" in
  gateway|openai|all) ;;
  *)
    echo "ERROR: IMAGE_RECORD_SET must be gateway, openai, or all" >&2
    exit 1
    ;;
esac

validate_input_fixtures

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
  record_provider_suite OpenAI --openai https://api.openai.com "$OPENAI_MODEL" "$OPENAI_MODEL_SLUG"
fi

if [[ "$IMAGE_RECORD_SET" == "gateway" || "$IMAGE_RECORD_SET" == "all" ]]; then
  record_provider_suite Gateway --gateway "$GATEWAY_URL" "$MODEL" "$MODEL_SLUG"
fi

promote_recorded_suite
