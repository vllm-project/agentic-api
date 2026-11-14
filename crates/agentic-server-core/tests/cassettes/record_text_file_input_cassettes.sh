#!/usr/bin/env bash
# Records inline text-file input conversations. The gateway set is recorded
# through Agentic API (client -> gateway -> vLLM hosting an open-source text
# model); the OpenAI reference set sends the same file to the OpenAI
# Responses API. Every turn carries `notes.txt` as an inline `input_file`, and
# the release codeword appears only inside that file, so a recording proves the
# model read the attachment.
#
# Scenarios:
#   single         one user message with a question and the inline file,
#                  recorded streaming and non-streaming
#   continuation   the single turn, then a text follow-up chained by
#                  previous_response_id, streaming and non-streaming
#   compact        the model-facing summary request the gateway builds when it
#                  compacts that file (decoded text plus the checkpoint prompt),
#                  then a follow-up on the compacted window built from the
#                  recorded summary; non-streaming, like tests/cassettes/compaction
#   files-text     Files API: upload notes.txt, retrieve it, reference it by
#                  `input_file.file_id`, continue by previous_response_id,
#                  delete it and retry, and use it as `input_image` (rejected)
#   files-image    Files API: the same lifecycle for `red-blue-64.png` through
#                  `input_image.file_id`, including the wrong-type rejection
#   files-errors   invalid uploads (purpose, missing file, missing purpose) and
#                  the missing-file errors for metadata, content, and delete
#
# The files scenarios are recorded with `record_cassette.py --mode files`; the
# gateway set needs the text model for files-text/files-errors and
# FILES_IMAGE_MODEL for files-image. Select them explicitly:
#   TEXT_FILE_SCENARIOS="files-text files-image files-errors"
#
# The compaction summary and follow-up are recorded directly against the model
# server (VLLM_URL, files named `vllm`) or OpenAI, because the gateway makes
# those calls internally; the replay test checks that the gateway builds exactly
# the recorded requests from the original input_file.
#
# Usage from the repository root (see README.md "Text file input"):
#   GATEWAY_URL=http://localhost:9000 VLLM_URL=http://localhost:8000 MODEL=Qwen/Qwen2.5-3B-Instruct \
#     TEXT_FILE_RECORD_SET=gateway bash crates/agentic-server-core/tests/cassettes/record_text_file_input_cassettes.sh
#   OPENAI_API_KEY=sk-... TEXT_FILE_RECORD_SET=openai bash .../record_text_file_input_cassettes.sh

set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="${TEXT_FILE_OUTPUT_DIR:-$SCRIPTS_DIR/text_files/responses}"
INPUT_DIR="$SCRIPTS_DIR/text_files/inputs"
RECORDER="${RECORDER:-$SCRIPTS_DIR/record_cassette.py}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
VLLM_URL="${VLLM_URL:-http://localhost:8000}"
MODEL="${MODEL:-Qwen/Qwen2.5-3B-Instruct}"
MODEL_SLUG="$(echo "$MODEL" | tr '/: ' '---')"
FILES_IMAGE_MODEL="${FILES_IMAGE_MODEL:-Qwen/Qwen2.5-VL-3B-Instruct}"
FILES_IMAGE_MODEL_SLUG="$(echo "$FILES_IMAGE_MODEL" | tr '/: ' '---')"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4o}"
OPENAI_MODEL_SLUG="$(echo "$OPENAI_MODEL" | tr '/: ' '---')"
TEXT_FILE_RECORD_SET="${TEXT_FILE_RECORD_SET:-all}"
TEXT_FILE_SCENARIOS="${TEXT_FILE_SCENARIOS:-single continuation compact}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-64}"
SUMMARY_MAX_OUTPUT_TOKENS="${SUMMARY_MAX_OUTPUT_TOKENS:-256}"
MARKER='PAPAYA-7'
FOLLOW_UP_PROMPT='What was the release codeword in the file I attached earlier? Reply with only the codeword.'
COMPACT_FOLLOW_UP_PROMPT='Reply with only the release codeword from the checkpoint.'
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

# The JSON turns must embed exactly the committed notes.txt, as a base64 file in
# the client turn and as the gateway's decoded `File: notes.txt` text in the
# summary request, so a recording can never drift from the replay fixtures.
validate_input_fixtures() {
  python - "$INPUT_DIR" "$MARKER" <<'PY'
import base64
import json
import sys
from pathlib import Path

inputs = Path(sys.argv[1])
marker = sys.argv[2]
notes = (inputs / "notes.txt").read_bytes()
if marker.encode() not in notes:
    raise SystemExit(f"ERROR: notes.txt must contain the marker {marker}")
single = json.loads((inputs / "single-file.json").read_text(encoding="utf-8"))
parts = single[0]["content"]
if [part["type"] for part in parts] != ["input_text", "input_file"]:
    raise SystemExit("ERROR: single-file.json must hold one input_text and one input_file part")
if marker in parts[0]["text"]:
    raise SystemExit("ERROR: the question must not contain the marker; it may appear only in the file")
expected = {"type": "input_file", "filename": "notes.txt",
            "file_data": "data:text/plain;base64," + base64.b64encode(notes).decode("ascii")}
if parts[1] != expected:
    raise SystemExit("ERROR: single-file.json does not embed notes.txt as its input_file part")
compact = json.loads((inputs / "compact-text-file.json").read_text(encoding="utf-8"))
if compact[0]["content"][1] != {"type": "input_text", "text": "File: notes.txt\n" + notes.decode("utf-8")}:
    raise SystemExit("ERROR: compact-text-file.json must carry notes.txt as the gateway's decoded input_text")
if marker in compact[0]["content"][0]["text"]:
    raise SystemExit("ERROR: the compaction instruction must not contain the marker")
PY
}

validate_recorded_scenario() {
  local scenario="$1"
  local file="$2"
  local stream_flag="$3"
  local input_fixture="$4"

  python - "$scenario" "$file" "$stream_flag" "$input_fixture" "$MARKER" "$FOLLOW_UP_PROMPT" <<'PY'
import json
import sys
from pathlib import Path

import yaml

scenario, path, stream_flag, input_fixture, marker, follow_up = sys.argv[1:7]
path = Path(path)
streaming = stream_flag == "--stream"
document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
turns = document.get("turns") or []
expected_turns = 2 if scenario == "continuation" else 1
if len(turns) != expected_turns:
    raise SystemExit(f"ERROR: expected {expected_turns} recorded turn(s) in {path}, found {len(turns)}")


def request_body(turn: dict) -> dict:
    return (turn.get("request") or {}).get("body") or {}


def terminal_response(turn: dict) -> dict:
    response = turn.get("response") or {}
    if response.get("status_code") != 200:
        raise SystemExit(f"ERROR: recording returned HTTP {response.get('status_code')}: {response.get('body')}")
    if not streaming:
        return response.get("body") or {}
    events = []
    for raw in response.get("sse") or []:
        for line in raw.splitlines():
            if line.startswith("data: ") and line != "data: [DONE]":
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


def answer_with_marker(terminal: dict, label: str) -> str:
    if terminal.get("status") != "completed":
        raise SystemExit(f"ERROR: {label} did not complete: {terminal}")
    text = "".join(
        part.get("text", "")
        for item in terminal.get("output") or []
        if item.get("type") == "message"
        for part in item.get("content") or []
        if part.get("type") == "output_text"
    )
    if marker not in text:
        raise SystemExit(f"ERROR: {label} answer does not contain {marker}; the model did not read the file: {text!r}")
    print(f"{label} answer: {text.strip()!r}")
    return text


for index, turn in enumerate(turns, start=1):
    if request_body(turn).get("stream") is not streaming:
        raise SystemExit(f"ERROR: turn {index} stream mode differs from {streaming}")

first = turns[0]
if request_body(first).get("input") != json.loads(Path(input_fixture).read_text(encoding="utf-8")):
    raise SystemExit(f"ERROR: turn 1 request input differs from {input_fixture}")
first_terminal = terminal_response(first)
answer_with_marker(first_terminal, "turn 1")

if scenario == "continuation":
    second = turns[1]
    second_request = request_body(second)
    if second_request.get("previous_response_id") != first_terminal.get("id"):
        raise SystemExit("ERROR: turn 2 previous_response_id does not reference turn 1")
    if second_request.get("input") != follow_up:
        raise SystemExit(f"ERROR: turn 2 request input is not the follow-up prompt: {second_request.get('input')!r}")
    answer_with_marker(terminal_response(second), "turn 2")
PY
}

# Build the follow-up input the gateway sends after compaction: the retained
# user message with the decoded file, the checkpoint as an assistant message,
# and the follow-up question.
write_compact_follow_up_input() {
  local summary_cassette="$1"
  local output="$2"

  python - "$summary_cassette" "$INPUT_DIR/compact-text-file.json" "$COMPACT_FOLLOW_UP_PROMPT" "$output" <<'PY'
import json
import sys
from pathlib import Path

import yaml

cassette, compact_input, prompt, output = sys.argv[1:5]
turn = yaml.safe_load(Path(cassette).read_text(encoding="utf-8"))["turns"][0]
summary = "".join(
    part.get("text", "")
    for item in turn["response"]["body"].get("output") or []
    if item.get("type") == "message"
    for part in item.get("content") or []
    if part.get("type") == "output_text"
)
retained = json.loads(Path(compact_input).read_text(encoding="utf-8"))[0]
follow_up = [
    retained,
    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": summary}]},
    {"type": "message", "role": "user", "content": prompt},
]
Path(output).write_text(json.dumps(follow_up, indent=2) + "\n", encoding="utf-8")
PY
}

run_recorder() {
  local output="$1"
  local stdin_lines="$2"
  shift 2

  if ! printf '%s' "$stdin_lines" | python "$RECORDER" --mode responses --output "$output" "$@"; then
    rm -f -- "$output"
    return 1
  fi
}

stage() {
  local temporary_output="$1"
  local output="$2"
  local staged_output

  staged_output="$STAGING_DIR/$(basename "$output")"
  mv -- "$temporary_output" "$staged_output"
  STAGED_OUTPUTS+=("$staged_output")
  FINAL_OUTPUTS+=("$output")
  green "✓ cassette validated -> $output"
}

record_conversation() {
  local scenario="$1"
  local endpoint_flag="$2"
  local endpoint="$3"
  local model="$4"
  local output="$5"
  local stream_flag="$6"
  local temporary_output
  local turns=1
  local stdin_lines=''

  if [[ "$scenario" == "continuation" ]]; then
    turns=2
    stdin_lines="$FOLLOW_UP_PROMPT"$'\n'
  fi
  temporary_output="$(mktemp "$STAGING_DIR/.text-file-cassette.XXXXXX")"
  run_recorder "$temporary_output" "$stdin_lines" "$stream_flag" --model "$model" "$endpoint_flag" "$endpoint" \
    --max-output-tokens "$MAX_OUTPUT_TOKENS" --turns "$turns" --input-file "$INPUT_DIR/single-file.json"
  if ! validate_recorded_scenario "$scenario" "$temporary_output" "$stream_flag" "$INPUT_DIR/single-file.json"; then
    rm -f -- "$temporary_output"
    return 1
  fi
  stage "$temporary_output" "$output"
}

record_compaction() {
  local endpoint_flag="$1"
  local endpoint="$2"
  local model="$3"
  local summary_output="$4"
  local follow_up_output="$5"
  local summary_file
  local follow_up_file
  local follow_up_input="$STAGING_DIR/.compact-follow-up-input.json"

  summary_file="$(mktemp "$STAGING_DIR/.text-file-cassette.XXXXXX")"
  run_recorder "$summary_file" '' --no-stream --no-store --model "$model" "$endpoint_flag" "$endpoint" \
    --max-output-tokens "$SUMMARY_MAX_OUTPUT_TOKENS" --turns 1 --input-file "$INPUT_DIR/compact-text-file.json"
  if ! validate_recorded_scenario compact "$summary_file" --no-stream "$INPUT_DIR/compact-text-file.json"; then
    rm -f -- "$summary_file"
    return 1
  fi
  write_compact_follow_up_input "$summary_file" "$follow_up_input"
  follow_up_file="$(mktemp "$STAGING_DIR/.text-file-cassette.XXXXXX")"
  run_recorder "$follow_up_file" '' --no-stream --no-store --model "$model" "$endpoint_flag" "$endpoint" \
    --max-output-tokens "$MAX_OUTPUT_TOKENS" --turns 1 --input-file "$follow_up_input"
  if ! validate_recorded_scenario compact "$follow_up_file" --no-stream "$follow_up_input"; then
    rm -f -- "$follow_up_file"
    return 1
  fi
  stage "$summary_file" "$summary_output"
  stage "$follow_up_file" "$follow_up_output"
}

# The Files API scenarios record a lifecycle rather than a single answer. A
# successful reference response must answer from the uploaded content (the
# marker or the two image colors); error turns are evidence in their own right
# and are printed rather than rejected, so a provider-specific status, code, or
# message is recorded instead of turning into a mismatch.
validate_files_scenario() {
  local scenario="$1"
  local file="$2"
  local provider="$3"

  python - "$scenario" "$file" "$provider" "$MARKER" <<'PY'
import json
import sys
from pathlib import Path

import yaml

scenario, path, provider, marker = sys.argv[1:5]
turns = (yaml.safe_load(Path(path).read_text(encoding="utf-8")) or {}).get("turns") or []
if not turns:
    raise SystemExit(f"ERROR: {path} recorded no turns")

expected_words = {
    "text": [marker],
    "image": ["red", "blue"],
    "errors": [],
}[scenario]


def response_text(turn: dict) -> str:
    body = (turn.get("response") or {}).get("body") or {}
    return "".join(
        part.get("text", "")
        for item in body.get("output") or []
        if isinstance(item, dict) and item.get("type") == "message"
        for part in item.get("content") or []
        if isinstance(part, dict) and part.get("type") == "output_text"
    )


answers = []
errors = []
uploads = 0
for index, turn in enumerate(turns, start=1):
    request = turn.get("request") or {}
    response = turn.get("response") or {}
    status = response.get("status_code")
    if request.get("path") == "/v1/files" and request.get("method") == "POST" and status == 200:
        uploads += 1
    if scenario == "errors":
        if status is None or status < 400:
            raise SystemExit(f"ERROR: files-errors turn {index} is not an error response: HTTP {status}")
        errors.append((index, status, response.get("body")))
        continue
    if request.get("path") != "/v1/responses":
        continue
    if status == 200:
        text = response_text(turn)
        answers.append(text)
        missing = [word for word in expected_words if word.lower() not in text.lower()]
        if missing:
            raise SystemExit(
                f"ERROR: turn {index} answer is missing {missing}; the model did not read the file: {text!r}"
            )
    else:
        errors.append((index, status, response.get("body")))

if scenario != "errors" and uploads == 0:
    raise SystemExit(f"ERROR: {path} has no successful Files API upload")
if scenario != "errors" and provider == "gateway" and len(answers) < 2:
    raise SystemExit(
        f"ERROR: the gateway recording must answer both the file reference and its continuation; "
        f"got {len(answers)} successful response(s)"
    )

for text in answers:
    print(f"answer: {text.strip()!r}")
for index, status, body in errors:
    print(f"  [turn {index} -> HTTP {status}: {json.dumps(body, sort_keys=True)}]")
PY
}

record_files_scenario() {
  local scenario="$1"
  local provider="$2"
  local endpoint_flag="$3"
  local endpoint="$4"
  local model="$5"
  local output="$6"
  local temporary_output

  temporary_output="$(mktemp "$STAGING_DIR/.text-file-cassette.XXXXXX")"
  if ! python "$RECORDER" --mode files --files-scenario "$scenario" --turns 1 --model "$model" \
    "$endpoint_flag" "$endpoint" --max-output-tokens "$MAX_OUTPUT_TOKENS" --output "$temporary_output"; then
    rm -f -- "$temporary_output"
    return 1
  fi
  if ! validate_files_scenario "$scenario" "$temporary_output" "$provider"; then
    rm -f -- "$temporary_output"
    return 1
  fi
  stage "$temporary_output" "$output"
}

record_provider_suite() {
  local provider="$1"
  local conversation_flag="$2"
  local conversation_endpoint="$3"
  local model_flag="$4"
  local model_endpoint="$5"
  local model="$6"
  local model_slug="$7"
  local compact_label="$8"
  local files_image_model="$OPENAI_MODEL"
  [[ "$provider" == "gateway" ]] && files_image_model="$FILES_IMAGE_MODEL"
  local files_image_slug
  files_image_slug="$(echo "$files_image_model" | tr '/: ' '---')"
  local scenario
  local mode

  bold "$provider text-file cassettes ($model, scenarios: $TEXT_FILE_SCENARIOS)"
  for scenario in $TEXT_FILE_SCENARIOS; do
    case "$scenario" in
      single|continuation)
        for mode in streaming nonstreaming; do
          bold "$provider $scenario ($mode)"
          local flag=--no-stream
          [[ "$mode" == "streaming" ]] && flag=--stream
          record_conversation "$scenario" "$conversation_flag" "$conversation_endpoint" "$model" \
            "$BASE_DIR/text-file-${scenario}-${provider}-${model_slug}-${mode}.yaml" "$flag"
        done
        ;;
      compact)
        bold "$provider compact (non-streaming)"
        record_compaction "$model_flag" "$model_endpoint" "$model" \
          "$BASE_DIR/text-file-compact-${compact_label}-${model_slug}-nonstreaming.yaml" \
          "$BASE_DIR/text-file-compact-followup-${compact_label}-${model_slug}-nonstreaming.yaml"
        ;;
      files-text)
        bold "$provider files API: text reference lifecycle (non-streaming)"
        record_files_scenario text "$provider" "$conversation_flag" "$conversation_endpoint" "$model" \
          "$BASE_DIR/files-api-text-${provider}-${model_slug}.yaml"
        ;;
      files-image)
        bold "$provider files API: image reference lifecycle (non-streaming)"
        record_files_scenario image "$provider" "$conversation_flag" "$conversation_endpoint" \
          "$files_image_model" "$BASE_DIR/files-api-image-${provider}-${files_image_slug}.yaml"
        ;;
      files-errors)
        bold "$provider files API: invalid uploads and missing files (non-streaming)"
        record_files_scenario errors "$provider" "$conversation_flag" "$conversation_endpoint" "$model" \
          "$BASE_DIR/files-api-errors-${provider}-${model_slug}.yaml"
        ;;
      *)
        echo "ERROR: unknown scenario $scenario" >&2
        return 1
        ;;
    esac
  done
}

promote_recorded_suite() {
  local index

  for index in "${!STAGED_OUTPUTS[@]}"; do
    mv -- "${STAGED_OUTPUTS[$index]}" "${FINAL_OUTPUTS[$index]}"
    chmod 644 -- "${FINAL_OUTPUTS[$index]}"
    green "✓ text-file cassette promoted -> ${FINAL_OUTPUTS[$index]}"
  done
}

case "$TEXT_FILE_RECORD_SET" in
  gateway|openai|all) ;;
  *)
    echo "ERROR: TEXT_FILE_RECORD_SET must be gateway, openai, or all" >&2
    exit 1
    ;;
esac

validate_input_fixtures

# Validate OpenAI requirements before any live request. Final fixtures remain
# unchanged until every selected recording has passed validation.
if [[ "$TEXT_FILE_RECORD_SET" != "gateway" && -z "${OPENAI_API_KEY:-}" ]]; then
  echo "ERROR: OPENAI_API_KEY must be set for TEXT_FILE_RECORD_SET=$TEXT_FILE_RECORD_SET" >&2
  exit 1
fi

mkdir -p "$BASE_DIR"
STAGING_DIR="$(mktemp -d "$BASE_DIR/.text-file-suite.XXXXXX")"

if [[ "$TEXT_FILE_RECORD_SET" != "gateway" ]]; then
  record_provider_suite openai --openai https://api.openai.com --openai https://api.openai.com \
    "$OPENAI_MODEL" "$OPENAI_MODEL_SLUG" openai
fi

if [[ "$TEXT_FILE_RECORD_SET" != "openai" ]]; then
  record_provider_suite gateway --gateway "$GATEWAY_URL" --vllm "$VLLM_URL" "$MODEL" "$MODEL_SLUG" vllm
fi

promote_recorded_suite
