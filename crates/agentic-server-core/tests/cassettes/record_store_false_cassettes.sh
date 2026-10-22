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
# Records JSON and SSE: full-history replay, stored/unstored parent references,
# explicit conversations, item-listing persistence probes, and a client-executed
# function with full-history replay.
# Input fixtures are derived from actual captured outputs. Captures from
# record_cassette.py are grouped without changing their recorded exchanges.
# Use --combine-existing to group existing captures without contacting providers.
set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/store_false"

combine_cassettes() {
  python - "$1" "$2" <<'PY'
import sys, textwrap, yaml
from pathlib import Path
source, destination = map(Path, sys.argv[1:])
scenarios = ('manual-replay', 'stored-parent', 'unstored-parent',
             'conversation', 'function-replay', 'conversation-items')
originals = []
bundles = []
for first in sorted(source.glob('store-false-manual-replay-*.yaml')):
    suffix = first.name.removeprefix('store-false-manual-replay-')
    expected = {}
    sections = ['cases:\n']
    for scenario in scenarios:
        path = source / f'store-false-{scenario}-{suffix}'
        captured = path.read_text()
        expected[scenario] = yaml.safe_load(captured)
        sections.append(f'  {scenario}:\n' + textwrap.indent(captured, '    '))
        originals.append(path)
    combined = ''.join(sections)
    assert yaml.safe_load(combined) == {'cases': expected}, suffix
    bundles.append((destination / f'store-false-{suffix}', combined))
assert bundles, f'No individual store:false captures found in {source}'
for path, combined in bundles:
    path.write_text(combined)
for path in originals:
    path.unlink()
print(f'Combined {len(originals)} captures into {len(bundles)} files in {destination}')
PY
}

case "${1:-}" in
  --combine-existing) combine_cassettes "$BASE_DIR" "$BASE_DIR"; exit 0 ;;
  '') ;;
  *) echo 'Usage: record_store_false_cassettes.sh [--combine-existing]' >&2; exit 1 ;;
esac

STORE_FALSE_RECORD_SET="${STORE_FALSE_RECORD_SET:-openai}"
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
STAGING_DIR="$(mktemp -d "$BASE_DIR/.recording.XXXXXX")"
trap 'rm -rf -- "$STAGING_DIR"' EXIT
RECORDER=(python "$SCRIPTS_DIR/record_cassette.py" "${BACKEND[@]}" --model "$MODEL" --max-output-tokens "$MAX_OUTPUT_TOKENS")
FIRST_PROMPT='Remember the word ORCHID. Reply with exactly OK.'
SECOND_PROMPT='What word did I ask you to remember? Reply with that word only.'

# Read a recorded response ID, or construct the next request's replayable input.
# This helper does not contact a provider or write a cassette.
captured_input() {
  python - "$1" "$2" "$TOOL_OUTPUTS" "$SECOND_PROMPT" <<'PY'
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
  python - "$output" <<'PY'
import sys, yaml
response = yaml.safe_load(open(sys.argv[1]))['turns'][-1]['response']
assert response['status_code'] == 400, response
assert response['body']['error']['code'] == 'previous_response_not_found', response
PY
}

for stream_mode in nonstreaming streaming; do
  STREAM_FLAG=--no-stream
  [[ "$stream_mode" == streaming ]] && STREAM_FLAG=--stream

  output="$STAGING_DIR/store-false-manual-replay-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --output "$output"
  captured_input "$output" replay > "$STAGING_DIR/input.json"
  "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" --input-file "$STAGING_DIR/input.json"

  output="$STAGING_DIR/store-false-stored-parent-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --output "$output"
  parent_id="$(captured_input "$output" id)"
  printf '%s\n' "$SECOND_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" \
    --request-overrides "{\"previous_response_id\":\"$parent_id\"}"
  child_id="$(captured_input "$output" id)"
  missing_parent "$output" "$child_id"

  output="$STAGING_DIR/store-false-unstored-parent-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --output "$output"
  parent_id="$(captured_input "$output" id)"
  missing_parent "$output" "$parent_id" --no-store

  output="$STAGING_DIR/store-false-conversation-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf '%s\n' "$FIRST_PROMPT" | "${RECORDER[@]}" --mode conv --turns 1 "$STREAM_FLAG" --no-store --output "$output"
  conversation_id="$(captured_input "$output" conversation-id)"
  printf '%s\n' "$SECOND_PROMPT" | "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" \
    --request-overrides "{\"conversation\":\"$conversation_id\"}"

  output="$STAGING_DIR/store-false-function-replay-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  printf 'Call get_job_status for job-382, then reply with just its status.\n' | "${RECORDER[@]}" --mode responses --turns 1 \
    "$STREAM_FLAG" --no-store --output "$output" --tools "$TOOLS" --tool-choice '{"type":"function","name":"get_job_status"}'
  captured_input "$output" tool-replay > "$STAGING_DIR/input.json"
  "${RECORDER[@]}" --mode responses --turns 1 "$STREAM_FLAG" --no-store --append --output "$output" \
    --tools "$TOOLS" --tool-choice none --input-file "$STAGING_DIR/input.json"

  output="$STAGING_DIR/store-false-conversation-items-${STORE_FALSE_RECORD_SET}-${MODEL_SLUG}-${stream_mode}.yaml"
  "${RECORDER[@]}" --mode items --items-scenario store-false --turns 11 "$STREAM_FLAG" --output "$output"
done

combine_cassettes "$STAGING_DIR" "$BASE_DIR"
