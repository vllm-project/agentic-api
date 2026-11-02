#!/usr/bin/env bash
# Record HTTP store:false cases through the existing record_cassette.py proxy.
#
# Usage from the repository root, with OPENAI_API_KEY exported:
#   uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
#     bash crates/agentic-server-core/tests/cassettes/record_store_false_cassettes.sh
#   MODEL=gpt-4o bash crates/agentic-server-core/tests/cassettes/record_store_false_cassettes.sh
#   STORE_FALSE_RECORD_SET=gateway GATEWAY_URL=http://127.0.0.1:9000 \
#     GATEWAY_MODEL=Qwen/Qwen3.8-27B-FP8 \
#     bash crates/agentic-server-core/tests/cassettes/record_store_false_cassettes.sh
#
# STORE_FALSE_SUITE=responses (default) records the cases below.
# STORE_FALSE_SUITE=multi-agent replays existing multi-agent cassettes through
# the selected OpenAI endpoint or running gateway, using full-history continuation.
# MULTI_AGENT_SUITE=all includes mixed-tools, tool-search, code-interpreter,
# and client-agent-message. The tool-search case also exercises hosted MCP search.
# MULTI_AGENT_MAX_CONTINUATIONS bounds client-tool follow-ups (default: 10, range: 1..100).
#
# Records JSON and SSE: full-history replay, stored/unstored parent references,
# explicit conversations, item-listing persistence probes, and a client-executed
# function with full-history replay.
# Each scenario has its own standard turns-based YAML from record_cassette.py.
# Follow-up input is derived from actual captured outputs.
set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/store_false"

if (( $# > 0 )); then
  echo 'Usage: record_store_false_cassettes.sh' >&2
  exit 1
fi

record_multi_agent() {
  local model provider mode output model_slug scenario
  local -a scenarios modes
  local -a backend
  case "$STORE_FALSE_RECORD_SET" in
    openai)
      : "${OPENAI_API_KEY:?OPENAI_API_KEY must be set}"
      model="${OPENAI_MODEL:-gpt-5.6-sol}"
      provider=openai-reference
      backend=(--openai https://api.openai.com)
      ;;
    gateway)
      model="${GATEWAY_MODEL:-${MODEL:-}}"
      : "${model:?Set GATEWAY_MODEL to the served model}"
      provider=gateway
      backend=(--gateway "${GATEWAY_URL:-http://127.0.0.1:9000}")
      ;;
    *) echo 'ERROR: STORE_FALSE_RECORD_SET must be openai or gateway' >&2; exit 1 ;;
  esac
  model_slug="$(printf '%s' "$model" | tr '/: ' '---')"
  mkdir -p "${MULTI_AGENT_OUTPUT_DIR:-$BASE_DIR}"
  case "${MULTI_AGENT_SUITE:-mixed-tools}" in
    all) scenarios=(mixed-tools tool-search code-interpreter client-agent-message) ;;
    mixed-tools|tool-search|code-interpreter|client-agent-message)
      scenarios=("${MULTI_AGENT_SUITE:-mixed-tools}") ;;
    *) echo 'ERROR: unknown MULTI_AGENT_SUITE' >&2; exit 1 ;;
  esac
  case "${MULTI_AGENT_STREAM_MODE:-both}" in
    both) modes=(nonstreaming streaming) ;;
    nonstreaming|streaming) modes=("$MULTI_AGENT_STREAM_MODE") ;;
    *) echo 'ERROR: MULTI_AGENT_STREAM_MODE must be both, nonstreaming, or streaming' >&2; exit 1 ;;
  esac
  for scenario in "${scenarios[@]}"; do
    for mode in "${modes[@]}"; do
      local stream_flag=--no-stream
      [[ "$mode" == streaming ]] && stream_flag=--stream
      output="${MULTI_AGENT_OUTPUT_DIR:-$BASE_DIR}/multi-agent-store-false-$provider-$scenario-$model_slug-$mode.yaml"
      MULTI_AGENT_SUITE="$scenario" "$PYTHON" "$SCRIPTS_DIR/record_cassette.py" "${backend[@]}" \
        --mode items --items-scenario multi-agent-store-false --turns 1 \
        --model "$model" "$stream_flag" --no-store \
        --http-read-timeout "${HTTP_READ_TIMEOUT:-900}" \
        --max-output-tokens "${MAX_OUTPUT_TOKENS:-8192}" \
        --proxy-port "${PROXY_PORT:-7070}" --output "$output"
    done
  done
}

STORE_FALSE_RECORD_SET="${STORE_FALSE_RECORD_SET:-openai}"
export PYTHON="${PYTHON:-python}"
case "${STORE_FALSE_SUITE:-responses}" in
  responses) ;;
  multi-agent) record_multi_agent; exit 0 ;;
  *) echo 'ERROR: STORE_FALSE_SUITE must be responses or multi-agent' >&2; exit 1 ;;
esac

case "$STORE_FALSE_RECORD_SET" in
  openai)
    : "${OPENAI_API_KEY:?OPENAI_API_KEY must be set}"
    MODEL="${MODEL:-gpt-4o}"
    MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-256}"
    BACKEND=(--openai https://api.openai.com)
    ;;
  gateway)
    MODEL="${GATEWAY_MODEL:-${MODEL:-}}"
    : "${MODEL:?Set GATEWAY_MODEL (or MODEL) to the model served by vLLM}"
    MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-2048}"
    BACKEND=(--gateway "${GATEWAY_URL:-http://127.0.0.1:9000}")
    ;;
  *)
    echo 'ERROR: STORE_FALSE_RECORD_SET must be openai or gateway' >&2
    exit 1
    ;;
esac
MODEL_SLUG="$(printf '%s' "$MODEL" | tr '/: ' '---')"
TOOLS="$SCRIPTS_DIR/tool_calls/multi_turn/pipeline_tools.json"
TOOL_OUTPUTS="$SCRIPTS_DIR/tool_calls/multi_turn/pipeline_tool_outputs.json"
mkdir -p "$BASE_DIR"
INPUT_DIR="$(mktemp -d)"
trap 'rm -rf -- "$INPUT_DIR"' EXIT
RECORDER=("$PYTHON" "$SCRIPTS_DIR/record_cassette.py" "${BACKEND[@]}" --model "$MODEL" --max-output-tokens "$MAX_OUTPUT_TOKENS")
FIRST_PROMPT='Remember the word ORCHID. Reply with exactly OK.'
SECOND_PROMPT='What word did I ask you to remember? Reply with that word only.'

# Read a recorded response ID, or construct the next request's replayable input.
# This helper does not contact a provider or write a cassette.
captured_input() {
  "$PYTHON" - "$1" "$2" "$TOOL_OUTPUTS" "$SECOND_PROMPT" <<'PY'
import json, sys, yaml
turns = yaml.safe_load(open(sys.argv[1]))['turns']
if sys.argv[2] == 'conversation-id':
    assert turns[0]['request']['path'] == '/v1/conversations'
    print(turns[0]['response']['body']['id'])
    sys.exit(0)
turn = turns[-1]
response = turn['response'].get('body')
if 'sse' in turn['response']:
    events = [json.loads(line[5:].strip()) for line in turn['response']['sse']
              if line.startswith('data:') and line[5:].strip() != '[DONE]']
    response = next(event['response'] for event in reversed(events) if event['type'] == 'response.completed')
assert response['status'] == 'completed', response
if sys.argv[2] == 'id':
    print(response['id'])
else:
    history = turn['request']['body']['input']
    if isinstance(history, str):
        history = [{'role': 'user', 'content': history}]
    history += response['output']
    if sys.argv[2] == 'tool-replay':
        outputs = json.load(open(sys.argv[3]))
        calls = [item for item in response['output'] if item['type'] == 'function_call']
        assert len(calls) == 1 and calls[0]['name'] == 'get_job_status', calls
        history += [{'type': 'function_call_output', 'call_id': call['call_id'], 'output': outputs[call['name']]} for call in calls]
    else:
        history += [{'role': 'user', 'content': sys.argv[4]}]
    print(json.dumps(history))
PY
}

missing_parent() {
  local output="$1"
  local previous_id="$2"
  shift 2
  if printf 'Say OK.\n' | "${RECORDER[@]}" --mode responses --turns 1 \
      "$STREAM_FLAG" --append --output "$output" \
      --request-overrides "{\"previous_response_id\":\"$previous_id\"}" "$@"; then
    echo 'ERROR: expected a rejected continuation of an unstored response' >&2
    return 1
  fi
  "$PYTHON" - "$output" <<'PY'
import sys, yaml
response = yaml.safe_load(open(sys.argv[1]))['turns'][-1]['response']
assert response['status_code'] == 400, response
assert response['body']['error']['code'] == 'previous_response_not_found', response
PY
}

for stream_mode in nonstreaming streaming; do
  STREAM_FLAG=--no-stream
  [[ "$stream_mode" == streaming ]] && STREAM_FLAG=--stream

  output="$BASE_DIR/store-false-manual-replay-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --output "$output"
  captured_input "$output" replay > "$INPUT_DIR/input.json"
  "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" --input-file "$INPUT_DIR/input.json"

  output="$BASE_DIR/store-false-stored-parent-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --output "$output"
  parent_id="$(captured_input "$output" id)"
  printf '%s\n' "$SECOND_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" \
    --request-overrides "{\"previous_response_id\":\"$parent_id\"}"
  child_id="$(captured_input "$output" id)"
  missing_parent "$output" "$child_id"

  output="$BASE_DIR/store-false-unstored-parent-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --output "$output"
  parent_id="$(captured_input "$output" id)"
  missing_parent "$output" "$parent_id" --no-store

  output="$BASE_DIR/store-false-conversation-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode conv --turns 1 "$STREAM_FLAG" --no-store --output "$output"
  conversation_id="$(captured_input "$output" conversation-id)"
  printf '%s\n' "$SECOND_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" \
    --request-overrides "{\"conversation\":\"$conversation_id\"}"

  output="$BASE_DIR/store-false-function-replay-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf 'Call get_job_status for job-382, then reply with just its status.\n' | "${RECORDER[@]}" --mode responses --turns 1 \
    "$STREAM_FLAG" --no-store --output "$output" --tools "$TOOLS" --tool-choice '{"type":"function","name":"get_job_status"}'
  captured_input "$output" tool-replay > "$INPUT_DIR/input.json"
  "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" \
    --tools "$TOOLS" --tool-choice none --input-file "$INPUT_DIR/input.json"

  output="$BASE_DIR/store-false-conversation-items-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  "${RECORDER[@]}" --mode items --items-scenario store-false --turns 11 "$STREAM_FLAG" --output "$output"
done
