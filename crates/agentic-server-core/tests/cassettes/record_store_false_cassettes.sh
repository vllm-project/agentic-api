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
# STORE_FALSE_SUITE=hosted-calls records, in one JSON cassette, the hosted call replays the other store:false
# scenarios leave out: an inline code_interpreter_call, and the mcp_call, web_search_call and code_interpreter_call
# of a stored response sent back by id, as returned and as item_reference.
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

# Follow-up input for a by-id replay of the last recorded turn: its user message, its output items as returned
# (items) or as item_reference (refs), and the follow-up. Fails unless that turn completed with an ITEM_TYPE item.
hosted_replay_input() {
  "$PYTHON" - "$@" <<'PY'
import json, sys, yaml
path, mode, item_type, follow_up = sys.argv[1:]
turn = yaml.safe_load(open(path))['turns'][-1]
response = turn['response'].get('body') or {}
output = response.get('output') or []
if response.get('status') != 'completed' or not any(item.get('type') == item_type for item in output):
    sys.exit(f'no completed {item_type} to replay: {[item.get("type") for item in output]}')
sent = turn['request']['body']['input']
if isinstance(sent, str):
    sent = [{'type': 'message', 'role': 'user', 'content': sent}]
user = [item for item in sent if item.get('type', 'message') == 'message' and item.get('role') == 'user'][:1]
replayed = output if mode == 'items' else [{'type': 'item_reference', 'id': item['id']} for item in output]
print(json.dumps(user + replayed + [{'type': 'message', 'role': 'user', 'content': follow_up}]))
PY
}

record_hosted_calls() {
  local model provider output type item_type prompt follow_up tools first_choice input
  local -a backend extra
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
  mkdir -p "$BASE_DIR"
  output="$BASE_DIR/store-false-hosted-calls-$provider-$(printf '%s' "$model" | tr '/: ' '---')-nonstreaming.yaml"
  rm -f -- "$output"
  HOSTED_WORK_DIR="$(mktemp -d)"
  trap 'rm -rf -- "$HOSTED_WORK_DIR"' EXIT
  local work="$HOSTED_WORK_DIR"
  hosted_record() {
    local append=()
    [[ -s "$output" ]] && append=(--append)
    "$PYTHON" "$SCRIPTS_DIR/record_cassette.py" "${backend[@]}" --mode responses --no-stream --model "$model" \
      --max-output-tokens "${MAX_OUTPUT_TOKENS:-2048}" --proxy-port "${PROXY_PORT:-7070}" --output "$output" \
      ${append[@]+"${append[@]}"} "$@"
  }

  # The gitmcp tiktoken server of the hosted tool-search fixtures, declared directly (not deferred).
  "$PYTHON" - "$SCRIPTS_DIR/hosted_tool_search/inputs/tools-mcp-deferred.json" "$work/tools-mcp.json" <<'PY'
import json, sys
server = next(tool for tool in json.load(open(sys.argv[1])) if tool['type'] == 'mcp')
server.pop('defer_loading', None)
server.pop('server_description', None)
json.dump([server], open(sys.argv[2], 'w'), indent=2)
PY
  printf '["required", "none"]' > "$work/required-none.json"
  local code_tools="$SCRIPTS_DIR/code_interpreter/openai_tools.json"
  [[ "$provider" == gateway ]] && code_tools="$SCRIPTS_DIR/code_interpreter/gateway_tools.json"
  local code_prompt code_follow_up
  code_prompt="$(cat "$SCRIPTS_DIR/code_interpreter/prompts.txt")"
  code_follow_up='Without calling a tool, answer from the code output above only: what exactly did the code print?'

  # Inline: t1 runs the code with store=false; t2 sends back t1's input and output items.
  printf '%s\n%s\n' "$code_prompt" "$code_follow_up" | hosted_record --turns 2 --no-store --manual-item-replay \
    --tools "$code_tools" --tool-choice-sequence "$work/required-none.json" --parallel-tool-calls false

  # By id: t1 is stored; t2 sends its output items as returned, t3 as item_reference, both with store=false.
  for type in mcp web-search code-interpreter; do
    extra=()
    case "$type" in
      mcp)
        item_type=mcp_call tools="$work/tools-mcp.json" first_choice=auto
        prompt='Use the tiktoken documentation tool to find which encoding gpt-4o uses. Answer in one sentence.'
        follow_up='Without calling a tool, answer from the documentation search result above only: which encoding did it report for gpt-4o?' ;;
      web-search)
        item_type=web_search_call tools="$SCRIPTS_DIR/web_search/tools.json" first_choice=auto
        prompt='Use web search to search for the exact query "potato", then summarize the result in one sentence.'
        follow_up='Without calling a tool, answer from the search result above only: name one source it returned.' ;;
      code-interpreter)
        item_type=code_interpreter_call tools="$code_tools" first_choice=required
        prompt="$code_prompt" follow_up="$code_follow_up"
        extra=(--parallel-tool-calls false) ;;
    esac
    printf '%s\n' "$prompt" | hosted_record --turns 1 --tools "$tools" --tool-choice "$first_choice" \
      ${extra[@]+"${extra[@]}"}
    hosted_replay_input "$output" items "$item_type" "$follow_up" > "$work/items.json"
    hosted_replay_input "$output" refs "$item_type" "$follow_up" > "$work/refs.json"
    for input in items refs; do
      hosted_record --turns 1 --no-store --input-file "$work/$input.json" --tools "$tools" --tool-choice none \
        ${extra[@]+"${extra[@]}"} < /dev/null
    done
  done
}

STORE_FALSE_RECORD_SET="${STORE_FALSE_RECORD_SET:-openai}"
export PYTHON="${PYTHON:-python}"
case "${STORE_FALSE_SUITE:-responses}" in
  responses) ;;
  multi-agent) record_multi_agent; exit 0 ;;
  hosted-calls) record_hosted_calls; exit 0 ;;
  *) echo 'ERROR: STORE_FALSE_SUITE must be responses, multi-agent or hosted-calls' >&2; exit 1 ;;
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
