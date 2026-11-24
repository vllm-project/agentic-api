#!/usr/bin/env bash
# Records max_tool_calls behavior for single-agent Responses requests, per
# https://github.com/vllm-project/agentic-api/issues/398.
#
# OpenAI documents max_tool_calls as "the maximum number of total calls to
# built-in tools that can be processed in a response", shared across every
# built-in tool, with further attempts ignored. These cassettes record what
# that means on the wire, from OpenAI as the reference and from the gateway.
#
# Each provider writes four groups. A group is one cassette per transport mode;
# every leg in it is an independent conversation appended with --append, so
# legs never share previous_response_id state. Legs appear in the order below.
#
#   validation (JSON only) -- one single-turn leg per value
#     null, 0, 1, -1, "2", 2.5, true, 2147483648, 4294967296, 9223372036854775807,
#     9223372036854775808, -9223372036854775808 sent verbatim with web_search declared.
#     Shows the accepted range (1..=i64::MAX) and error shape. Values beyond 64 bits are
#     left out: the Rust cassette loader cannot represent them.
#
#   builtin (JSON and SSE) -- exhaustion and failed calls for each built-in tool kind
#     sequential        tools-web-search.json, limit 1, parallel off: three searches, one at a time.
#     parallel          tools-web-search.json, limit 2, parallel on: four searches in one batch.
#     required          tools-web-search.json, limit 1, tool_choice=required: three searches.
#     mixed-builtin     tools-web-search-mcp.json, limit 2: two searches and two MCP calls. Shows
#                       whether web search and MCP share one budget and whether mcp_list_tools counts.
#     code-interpreter  tools-code-interpreter.json, limit 1: two separate executions.
#     code-interpreter-failure  tools-code-interpreter.json, limit 1: an execution that raises,
#                       then a retry. Shows whether a failed execution consumes the budget.
#     mcp-failure       tools-mcp-fetch.json, limit 1: an MCP call with an invalid argument type (MCP
#                       error -32602), then a retry. Shows whether a failed MCP call consumes the budget.
#     mcp-then-search   tools-web-search-mcp.json, limit 1: a search, an MCP call, then a second search,
#                       each attempted even after a limit error. Shows that a refused MCP call has no
#                       item while a later refused search is still shown at `searching`.
#
#   counting (JSON and SSE) -- which calls count toward the limit, and when the count starts over
#     client-only           limit 1, parallel on: three get_weather calls and no search.
#     search-then-function  limit 1: one search, then get_weather in the same response.
#     client-functions      limit 1 per turn, two turns: two get_weather calls and two searches,
#                           then the function outputs plus two more searches by previous_response_id.
#     continuation          limit [1, omitted], two turns: one search, then three searches with the
#                           field omitted. Shows whether previous_response_id inherits the limit.
#     exhausted-continuation  limit 1 per turn, two turns: two searches (one blocked), then one
#                           search by previous_response_id. Shows whether a re-sent limit starts a
#                           fresh budget after an exhausted parent, and continuing past a blocked call.
#
#   websocket -- the sequential leg over the WebSocket transport (--append is HTTP only)
#
# Usage from the repository root:
#
#   # OpenAI reference only (default)
#   OPENAI_API_KEY=sk-... \
#     bash crates/agentic-server-core/tests/cassettes/record_max_tool_calls_cassettes.sh
#
#   # Gateway only, against a running agentic-server. The gateway cannot run the
#   # MCP leg against an upstream that rejects mcp__ function names, and the
#   # code-interpreter leg needs an embedded-code-interpreter build.
#   MAX_TOOL_CALLS_RECORD_SET=gateway GATEWAY_URL=http://localhost:9000 GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B \
#   MAX_TOOL_CALLS_SKIP_LEGS="mixed-builtin mcp-failure mcp-then-search" \
#     bash crates/agentic-server-core/tests/cassettes/record_max_tool_calls_cassettes.sh
#
#   # Selected groups only
#   MAX_TOOL_CALLS_GROUPS="builtin counting" OPENAI_API_KEY=sk-... \
#     bash crates/agentic-server-core/tests/cassettes/record_max_tool_calls_cassettes.sh

set -uo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/max_tool_calls"
RECORDER="${RECORDER_PYTHON:-python}"
MAX_TOOL_CALLS_RECORD_SET="${MAX_TOOL_CALLS_RECORD_SET:-openai}"
MAX_TOOL_CALLS_GROUPS="${MAX_TOOL_CALLS_GROUPS:-validation builtin counting websocket}"
MAX_TOOL_CALLS_SKIP_LEGS="${MAX_TOOL_CALLS_SKIP_LEGS:-}"
OPENAI_MODEL_NAME="${OPENAI_MODEL:-gpt-5.6}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
GATEWAY_MODEL="${GATEWAY_MODEL:-Qwen/Qwen3.6-35B-A3B}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-4096}"
PROXY_PORT="${PROXY_PORT:-7070}"

BUILTIN_LEGS="sequential parallel required mixed-builtin code-interpreter code-interpreter-failure mcp-failure mcp-then-search"
COUNTING_LEGS="client-only search-then-function client-functions continuation exhausted-continuation"
VALIDATION_VALUES=(null 0 1 -1 '"2"' 2.5 true 2147483648 4294967296 9223372036854775807 9223372036854775808
  -9223372036854775808)

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }
red()   { printf '\033[31m%s\033[0m\n' "$*"; }

case "$MAX_TOOL_CALLS_RECORD_SET" in
  openai|gateway|all) ;;
  *)
    echo "ERROR: MAX_TOOL_CALLS_RECORD_SET must be openai, gateway, or all" >&2
    exit 1
    ;;
esac

if [[ "$MAX_TOOL_CALLS_RECORD_SET" != "gateway" && -z "${OPENAI_API_KEY:-}" ]]; then
  echo "ERROR: OPENAI_API_KEY must be set for MAX_TOOL_CALLS_RECORD_SET=$MAX_TOOL_CALLS_RECORD_SET" >&2
  exit 1
fi

VALIDATION_PROMPT='Use web search to search for the exact query "potato", then answer in one sentence.'
SEQUENTIAL_PROMPT='Run three separate web searches, one after another, each as its own web_search call: first the exact query "potato nutrition facts", then "tomato nutrition facts", then "carrot nutrition facts". Do not combine them into one search. Then summarize each result in one sentence.'
PARALLEL_PROMPT='Run four separate web searches in parallel in this single turn, each as its own web_search call with exactly one query: "potato nutrition facts", "tomato nutrition facts", "cucumber nutrition facts", and "carrot nutrition facts". Do not combine queries. Then summarize each result in one sentence.'
MIXED_PROMPT='Make four separate tool calls: two web searches, for the exact queries "latest vLLM release notes" and "latest PyTorch release notes", and two calls to gitmcp_tiktoken__search_tiktoken_documentation, with {"query":"encoding"} and {"query":"tokenizer"}. Then summarize each result in one sentence.'
CODE_PROMPT='Use the python tool twice, as two separate tool calls: first compute 2**100, then in a second call compute the sum of the integers from 1 to 1000. Report both results.'
CLIENT_ONLY_PROMPT='Call get_weather three times in parallel in this single turn, for "Tokyo", "Paris", and "Cairo". Do not search the web.'
SEARCH_THEN_FUNCTION_PROMPT='First run one web search for the exact query "capital of France". After that search returns, call get_weather for the capital city it names.'
CLIENT_TURN1_PROMPT='Make four separate tool calls now: call get_weather for "Tokyo", call get_weather for "Paris", and run two web searches for the exact queries "Tokyo events this week" and "Paris events this week". Then summarize what you found.'
CLIENT_TURN2_PROMPT='Now run two more separate web searches, for the exact queries "Tokyo museums" and "Paris museums", and summarize each in one sentence.'
CODE_FAILURE_PROMPT='Use the python tool to run exactly print(1/0). If that raises an error, make a second, separate python tool call that runs exactly print("retry ok"). Report what happened in each call.'
MCP_FAILURE_PROMPT='Call gitmcp_tiktoken__fetch_generic_url_content with exactly {"url": 12345} -- the url must be the number 12345, not a string. If it returns an error, retry once with a separate call using {"url":"https://retry.invalid/"}. Report what happened in each call.'
MCP_THEN_SEARCH_PROMPT='Make exactly these three tool calls, one at a time and in this order, and make every one of them even if an earlier call fails or reports a limit: (1) a web search for the exact query "potato nutrition facts"; (2) call gitmcp_tiktoken__search_tiktoken_documentation with {"query":"encoding"}; (3) a web search for the exact query "tomato nutrition facts". Then report what happened to each call.'
EXHAUSTED_TURN1_PROMPT='Run two separate web searches, one after another, each as its own web_search call: first the exact query "potato nutrition facts", then "tomato nutrition facts". Then summarize each result in one sentence.'
EXHAUSTED_TURN2_PROMPT='Now run one web search for the exact query "carrot nutrition facts" and summarize it in one sentence.'
CONTINUATION_TURN1_PROMPT='Use web search once to search for the exact query "potato", then answer in one sentence.'

# Sets LEG_TURNS, LEG_PROMPTS, LEG_TOOLS, and LEG_ARGS for one named leg.
define_leg() {
  local tools="$BASE_DIR/tools-web-search.json" client_tools="$BASE_DIR/tools-web-search-client.json"
  LEG_TURNS=1
  case "$1" in
    sequential)
      LEG_PROMPTS="$SEQUENTIAL_PROMPT" LEG_TOOLS="$tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    parallel)
      LEG_PROMPTS="$PARALLEL_PROMPT" LEG_TOOLS="$tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls true --max-tool-calls 2) ;;
    required)
      LEG_PROMPTS="$SEQUENTIAL_PROMPT" LEG_TOOLS="$tools"
      LEG_ARGS=(--tool-choice required --parallel-tool-calls false --max-tool-calls 1) ;;
    mixed-builtin)
      LEG_PROMPTS="$MIXED_PROMPT" LEG_TOOLS="$BASE_DIR/tools-web-search-mcp.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls true --max-tool-calls 2) ;;
    code-interpreter)
      LEG_PROMPTS="$CODE_PROMPT" LEG_TOOLS="$BASE_DIR/tools-code-interpreter.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    code-interpreter-failure)
      LEG_PROMPTS="$CODE_FAILURE_PROMPT" LEG_TOOLS="$BASE_DIR/tools-code-interpreter.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    mcp-failure)
      LEG_PROMPTS="$MCP_FAILURE_PROMPT" LEG_TOOLS="$BASE_DIR/tools-mcp-fetch.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    mcp-then-search)
      LEG_PROMPTS="$MCP_THEN_SEARCH_PROMPT" LEG_TOOLS="$BASE_DIR/tools-web-search-mcp.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    client-only)
      LEG_PROMPTS="$CLIENT_ONLY_PROMPT" LEG_TOOLS="$client_tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls true --max-tool-calls 1) ;;
    search-then-function)
      LEG_PROMPTS="$SEARCH_THEN_FUNCTION_PROMPT" LEG_TOOLS="$client_tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    client-functions)
      LEG_TURNS=2 LEG_PROMPTS="$(printf '%s\n%s' "$CLIENT_TURN1_PROMPT" "$CLIENT_TURN2_PROMPT")"
      LEG_TOOLS="$client_tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls true --max-tool-calls 1
        --tool-outputs "$BASE_DIR/tool_outputs.py") ;;
    continuation)
      LEG_TURNS=2 LEG_PROMPTS="$(printf '%s\n%s' "$CONTINUATION_TURN1_PROMPT" "$SEQUENTIAL_PROMPT")"
      LEG_TOOLS="$tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls '[1, null]') ;;
    exhausted-continuation)
      LEG_TURNS=2 LEG_PROMPTS="$(printf '%s\n%s' "$EXHAUSTED_TURN1_PROMPT" "$EXHAUSTED_TURN2_PROMPT")"
      LEG_TOOLS="$tools"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    *)
      echo "ERROR: unknown leg $1" >&2
      exit 1 ;;
  esac
}

assert_sanitized() {
  CASSETTE_PATH="$1" "$RECORDER" -c '
import os
from pathlib import Path

import yaml

secret = os.environ.get("OPENAI_API_KEY", "")
text = Path(os.environ["CASSETTE_PATH"]).read_text(encoding="utf-8")
if secret and secret in text:
    raise SystemExit("recorded cassette contains OPENAI_API_KEY material")
for turn in (yaml.safe_load(text) or {}).get("turns", []):
    authorization = turn.get("request", {}).get("headers", {}).get("authorization")
    if authorization is not None and authorization != "Bearer ***":
        raise SystemExit("recorded cassette contains an unmasked authorization header")
'
}

# Moves a staged recording into place after the credential check. A provider
# error is still useful evidence, so only an empty or unsanitized capture fails.
finalize() {
  local staged="$1" output="$2"
  if [[ ! -s "$staged" ]]; then
    rm -f -- "$staged"
    red "✗ no response was recorded for $output"
    return 1
  fi
  if ! assert_sanitized "$staged"; then
    rm -f -- "$staged"
    red "✗ discarded unsanitized recording for $output"
    return 1
  fi
  mv -- "$staged" "$output"
  green "✓ recorded -> $output"
}

# record_turns ENDPOINT_FLAG ENDPOINT MODEL TURNS PROMPTS TOOLS STREAM_FLAG STAGED [RECORDER_ARGS...]
# Appends to STAGED when it already holds an earlier leg.
record_turns() {
  local endpoint_flag="$1" endpoint="$2" model="$3" turns="$4" prompts="$5" tools_file="$6"
  local stream_flag="$7" staged="$8"
  shift 8
  local append_flag=()
  [[ -s "$staged" ]] && append_flag=(--append)

  printf '%s\n' "$prompts" \
    | "$RECORDER" "$SCRIPTS_DIR/record_cassette.py" \
        --mode responses \
        --turns "$turns" \
        "$stream_flag" \
        --model "$model" \
        "$endpoint_flag" "$endpoint" \
        --tools "$tools_file" \
        --max-output-tokens "$MAX_OUTPUT_TOKENS" \
        --proxy-port "$PROXY_PORT" \
        --output "$staged" \
        "${append_flag[@]}" \
        "$@"
}

skipped() {
  [[ " $MAX_TOOL_CALLS_SKIP_LEGS " == *" $1 "* ]]
}

selected() {
  [[ " $MAX_TOOL_CALLS_GROUPS " == *" $1 "* ]]
}

record_validation() {
  local endpoint_flag="$1" endpoint="$2" model="$3" output="$4"
  local staged value
  staged="$(mktemp -u "$BASE_DIR/.max-tool-calls-cassette.XXXXXX")"
  for value in "${VALIDATION_VALUES[@]}"; do
    bold "  max_tool_calls=$value"
    record_turns "$endpoint_flag" "$endpoint" "$model" 1 "$VALIDATION_PROMPT" \
      "$BASE_DIR/tools-web-search.json" --no-stream "$staged" \
      --tool-choice auto --request-overrides "{\"max_tool_calls\": $value}"
  done
  finalize "$staged" "$output"
}

# record_group ENDPOINT_FLAG ENDPOINT MODEL STREAM_FLAG OUTPUT LEG...
record_group() {
  local endpoint_flag="$1" endpoint="$2" model="$3" stream_flag="$4" output="$5"
  shift 5
  local staged leg
  staged="$(mktemp -u "$BASE_DIR/.max-tool-calls-cassette.XXXXXX")"
  for leg in "$@"; do
    if skipped "$leg"; then
      bold "  $leg (skipped)"
      continue
    fi
    bold "  $leg"
    define_leg "$leg"
    record_turns "$endpoint_flag" "$endpoint" "$model" "$LEG_TURNS" "$LEG_PROMPTS" "$LEG_TOOLS" \
      "$stream_flag" "$staged" "${LEG_ARGS[@]}"
  done
  finalize "$staged" "$output"
}

record_provider_suite() {
  local provider_label="$1" endpoint_flag="$2" endpoint="$3" model="$4" suffix="$5"
  local slug prefix
  slug="$(printf '%s' "$model" | tr '/: ' '---')"
  prefix="$BASE_DIR/max-tool-calls"

  bold "═══ $provider_label ($endpoint) — model: $model ═══"

  if selected validation; then
    bold "validation (nonstreaming)"
    record_validation "$endpoint_flag" "$endpoint" "$model" "$prefix-validation-$suffix-$slug-nonstreaming.yaml"
  fi

  local stream_flag mode group legs
  for stream_flag in --stream --no-stream; do
    mode="streaming"
    [[ "$stream_flag" == "--no-stream" ]] && mode="nonstreaming"
    for group in builtin counting; do
      selected "$group" || continue
      legs="$BUILTIN_LEGS"
      [[ "$group" == counting ]] && legs="$COUNTING_LEGS"
      bold "$group ($mode)"
      # shellcheck disable=SC2086 # legs is a space-separated list of leg names
      record_group "$endpoint_flag" "$endpoint" "$model" "$stream_flag" \
        "$prefix-$group-$suffix-$slug-$mode.yaml" $legs
    done
  done

  if selected websocket; then
    bold "websocket (sequential leg)"
    local staged
    staged="$(mktemp -u "$BASE_DIR/.max-tool-calls-cassette.XXXXXX")"
    define_leg sequential
    record_turns "$endpoint_flag" "$endpoint" "$model" "$LEG_TURNS" "$LEG_PROMPTS" "$LEG_TOOLS" \
      --stream "$staged" --transport websocket "${LEG_ARGS[@]}"
    finalize "$staged" "$prefix-websocket-$suffix-$slug.yaml"
  fi
}

mkdir -p "$BASE_DIR"

if [[ "$MAX_TOOL_CALLS_RECORD_SET" == "openai" || "$MAX_TOOL_CALLS_RECORD_SET" == "all" ]]; then
  record_provider_suite "OpenAI" --openai "https://api.openai.com" "$OPENAI_MODEL_NAME" "openai-reference"
fi

if [[ "$MAX_TOOL_CALLS_RECORD_SET" == "gateway" || "$MAX_TOOL_CALLS_RECORD_SET" == "all" ]]; then
  record_provider_suite "Gateway" --gateway "$GATEWAY_URL" "$GATEWAY_MODEL" "gateway"
fi

echo
green "max_tool_calls cassettes recorded -> $BASE_DIR"
