#!/usr/bin/env bash
# Records hosted tool search (`{"type": "tool_search"}`, `execution: "server"`),
# per https://github.com/vllm-project/agentic-api/issues/448.
#
# OpenAI runs hosted tool search in the same response: the model sees only the
# name and description of each deferred function, namespace, or MCP server, and
# the API loads the selected surfaces and emits `tool_search_call` and
# `tool_search_output` with `execution: "server"` and `call_id: null`. These
# cassettes record that contract from OpenAI as the reference, and from the
# gateway running the search itself.
#
# The OpenAI reference writes seven groups and the gateway six (no replay). A group is one cassette per mode;
# every leg in it is an independent conversation appended with --append, so
# legs never share previous_response_id state. Legs appear in the order below.
#
#   validation (JSON only) -- one single-turn leg per declaration or tool_choice edge
#     explicit-server        {"type":"tool_search","execution":"server"} with one deferred function.
#     no-deferred            tool_search with no deferred tool.
#     defer-without-search   deferred function and MCP server without a tool_search declaration.
#     tool-choice-function   tool_choice names the deferred get_weather before it is loaded.
#     tool-choice-mcp        tool_choice names the deferred MCP server before it is loaded.
#     tool-choice-required   tool_choice=required when every callable tool is deferred.
#
#   contract (JSON and SSE) -- hosted search output items and streaming events
#     mcp-deferred        deferred MCP server with server_description: when mcp_list_tools is emitted and
#                         what tool_search_output.tools contains for an MCP server.
#     mcp-no-description  deferred MCP server without server_description: the catalog description fallback.
#     mixed-catalog       three turns: deferred functions, a namespace, and an MCP server; the search loads a
#                         function and a namespace, whose calls return as client function calls.
#     mixed-parallel-off  mixed-catalog with parallel_tool_calls=false.
#     max-tool-calls      deferred MCP server, limit 1, two documentation searches: whether the hosted
#                         search counts toward max_tool_calls.
#     continuation        two turns by previous_response_id: whether a loaded MCP server stays loaded.
#
#   query (JSON only) -- how the optional `query` argument filters what `paths` load. Each leg asks the model
#   to call tool_search once with fixed arguments over tools-query.json (travel gains book_flight; the MCP
#   server allows three tools). The recorded tool_search_call holds the arguments the model actually sent.
#     query-exact-name         {"paths":["travel"],"query":"get_timezone"}
#     query-name-fragment      {"paths":["travel"],"query":"timezone"}
#     query-description-word   {"paths":["travel"],"query":"latitude"}   (only get_coordinates' description)
#     query-synonym            {"paths":["travel"],"query":"airplane ticket"}   (book_flight, no shared word)
#     query-two-words          {"paths":["travel"],"query":"timezone distance"}   (any word or all words)
#     query-uppercase          {"paths":["travel"],"query":"GET_TIMEZONE"}
#     query-no-match           {"paths":["travel"],"query":"chocolate cake recipe"}
#     query-empty              {"paths":["travel"],"query":""}
#     query-function-match     {"paths":["get_weather"],"query":"weather"}
#     query-function-mismatch  {"paths":["get_weather"],"query":"timezone"}
#     query-outside-paths      {"paths":["get_weather"],"query":"get_exchange_rate"}
#     query-without-paths      {"query":"timezone"}
#     query-mcp                {"paths":["mcp_gitmcp_tiktoken"],"query":"code"}
#     query-partial-word       {"paths":["travel"],"query":"coord"}   (inside a word only)
#     query-plural             {"paths":["travel"],"query":"city"}   (calculate_distance says "cities")
#     query-split-words        {"paths":["travel"],"query":"latitude city"}   (both words, not adjacent)
#     query-space-for-underscore {"paths":["travel"],"query":"get timezone"}
#     query-description-phrase {"paths":["travel"],"query":"time zone"}   (adjacent in the description)
#     query-parameter-name     {"paths":["travel"],"query":"destination_airport"}   (book_flight parameter)
#     query-unknown-path       {"paths":["no_such_tool"]}
#     query-empty-paths        {"paths":[],"query":"timezone"}
#
#   edges (JSON only) -- already-loaded paths, an unreachable MCP server, and a description-only function match
#     query-function-description {"paths":["get_weather"],"query":"CURRENT"} over tools-query.json (description only)
#     repeat-same-response   four searches in one response over tools-query.json: get_weather twice, then
#                            travel with query "timezone", then all of travel.
#     repeat-continuation    two turns by previous_response_id over tools-query.json: load travel with query
#                            "timezone", then search that again and all of travel.
#     mcp-unreachable-unused  deferred MCP server at an unresolvable host; the prompt needs only get_weather.
#     mcp-unreachable-used    the same catalog; the prompt needs the MCP server.
#     mcp-unreachable-eager   control: the same server without defer_loading; the prompt needs only get_weather.
#
#   mcp-failures (JSON and SSE) -- how a request with a failing MCP server ends. Every leg asks for the weather,
#   so the failing server is never needed; broken_docs is the failing server.
#     failure-unreachable           deferred server at an unresolvable host (mcp.invalid).
#     failure-unreachable-eager     the same server without defer_loading.
#     failure-http-500              deferred server answering HTTP 500 (httpbin.org/status/500).
#     failure-http-500-eager        the same server without defer_loading.
#     failure-not-mcp               deferred URL that is not an MCP server (example.com, HTTP 405).
#     failure-auth-required         deferred server that requires authorization (GitHub MCP, HTTP 401).
#     failure-empty-allowed         deferred gitmcp server whose allowed_tools names no listed tool.
#
#   replay (JSON only, OpenAI reference only) -- two turns with store=false that replay the hosted transcript as
#   input. The gateway sends a replayed mcp_call to the model unchanged, which vLLM rejects, so it has no leg yet.
#
#   websocket -- the mcp-deferred leg over the WebSocket transport (--append is HTTP only)
#
# Usage from the repository root:
#
#   # OpenAI reference only (default)
#   OPENAI_API_KEY=sk-... \
#     bash crates/agentic-server-core/tests/cassettes/record_hosted_tool_search_cassettes.sh
#
#   # Gateway only, against a running agentic-server that may reach the recorded MCP
#   # hosts (AGENTIC_MCP_ALLOWED_HOSTS=gitmcp.io,mcp.invalid,httpbin.org,example.com,api.githubcopilot.com)
#   HOSTED_TOOL_SEARCH_RECORD_SET=gateway GATEWAY_URL=http://localhost:9000 \
#     GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 MAX_OUTPUT_TOKENS=16384 \
#     bash crates/agentic-server-core/tests/cassettes/record_hosted_tool_search_cassettes.sh
#
#   # Selected groups or legs only
#   HOSTED_TOOL_SEARCH_GROUPS="contract" HOSTED_TOOL_SEARCH_SKIP_LEGS="max-tool-calls" OPENAI_API_KEY=sk-... \
#     bash crates/agentic-server-core/tests/cassettes/record_hosted_tool_search_cassettes.sh

set -uo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$SCRIPTS_DIR/hosted_tool_search"
INPUTS_DIR="$BASE_DIR/inputs"
RECORDER="${RECORDER_PYTHON:-python}"
HOSTED_TOOL_SEARCH_RECORD_SET="${HOSTED_TOOL_SEARCH_RECORD_SET:-openai}"
HOSTED_TOOL_SEARCH_GROUPS="${HOSTED_TOOL_SEARCH_GROUPS:-validation contract query edges mcp-failures replay websocket}"
HOSTED_TOOL_SEARCH_SKIP_LEGS="${HOSTED_TOOL_SEARCH_SKIP_LEGS:-}"
OPENAI_MODEL_NAME="${OPENAI_MODEL:-gpt-5.6}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
GATEWAY_MODEL="${GATEWAY_MODEL:-Qwen/Qwen3.6-35B-A3B-FP8}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-4096}"
PROXY_PORT="${PROXY_PORT:-7070}"

VALIDATION_LEGS="explicit-server no-deferred defer-without-search tool-choice-function tool-choice-mcp
  tool-choice-required"
CONTRACT_LEGS="mcp-deferred mcp-no-description mixed-catalog mixed-parallel-off max-tool-calls continuation"
QUERY_LEGS="query-exact-name query-name-fragment query-description-word query-synonym query-two-words
  query-uppercase query-no-match query-empty query-function-match query-function-mismatch query-outside-paths
  query-without-paths query-mcp query-partial-word query-plural query-split-words query-space-for-underscore
  query-description-phrase query-parameter-name query-unknown-path query-empty-paths"
MCP_FAILURE_LEGS="failure-unreachable failure-unreachable-eager failure-http-500 failure-http-500-eager
  failure-not-mcp failure-auth-required failure-empty-allowed"
EDGE_LEGS="query-function-description repeat-same-response repeat-continuation mcp-unreachable-unused
  mcp-unreachable-used mcp-unreachable-eager"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }
red()   { printf '\033[31m%s\033[0m\n' "$*"; }

case "$HOSTED_TOOL_SEARCH_RECORD_SET" in
  openai|gateway|all) ;;
  *)
    echo "ERROR: HOSTED_TOOL_SEARCH_RECORD_SET must be openai, gateway, or all" >&2
    exit 1
    ;;
esac

if [[ "$HOSTED_TOOL_SEARCH_RECORD_SET" != "gateway" && -z "${OPENAI_API_KEY:-}" ]]; then
  echo "ERROR: OPENAI_API_KEY must be set for HOSTED_TOOL_SEARCH_RECORD_SET=$HOSTED_TOOL_SEARCH_RECORD_SET" >&2
  exit 1
fi

WEATHER_PROMPT='What is the current weather in Paris? Use a tool.'
MCP_PROMPT='Use the tiktoken documentation tool to find which encoding gpt-4o uses. Answer in one sentence.'
MIXED_TURN1_PROMPT='Find the current weather in Paris with get_weather and the IANA time zone of Paris with the travel namespace tool get_timezone. Load both tools, then call both; do not answer from memory.'
MIXED_TURN2_PROMPT='If either get_weather or travel get_timezone has not been called yet, call it now; otherwise answer in one sentence.'
MIXED_TURN3_PROMPT='Use the tool outputs above and answer in one sentence. Call no more tools.'
MAX_TOOL_CALLS_PROMPT='Search the tiktoken documentation twice, as two separate calls one after another: first for the exact query "encoding_for_model", then for "o200k_base". Then report what each call returned.'
FOLLOW_UP_PROMPT='Now search the tiktoken documentation once more, for the exact query "how to install tiktoken", and answer in one sentence.'

REPEAT_SAME_RESPONSE_PROMPT='This is a test of the tool loader. Call tool_search four times, one call after another, with exactly these arguments in this order, unchanged: {"paths":["get_weather"]}, then {"paths":["get_weather"]}, then {"paths":["travel"],"query":"timezone"}, then {"paths":["travel"]}. Do not call any other tool. Then reply with only the names of the tools that each search loaded, or "none" for a search that loaded nothing.'
REPEAT_CONTINUATION_PROMPT='This is a test of the tool loader. Call tool_search twice, one call after another, with exactly these arguments in this order, unchanged: {"paths":["travel"],"query":"timezone"}, then {"paths":["travel"]}. Do not call any other tool. Then reply with only the names of the tools that each search loaded, or "none" for a search that loaded nothing.'
BROKEN_MCP_PROMPT='Use the broken_docs documentation tools to find how to configure the example project. Answer in one sentence.'

# The MCP failure legs share inputs/tools-mcp-unreachable.json and differ only in the broken_docs server:
# its URL, whether it is deferred, and an allowed_tools filter that matches nothing.
VARIANTS_DIR="$(mktemp -d)"
trap 'rm -rf -- "$VARIANTS_DIR"' EXIT

# broken_mcp_tools NAME SERVER_URL [eager] [no-match-filter]
# Writes that variant of the catalog and prints its path.
broken_mcp_tools() {
  local name="$1" url="$2"
  shift 2
  local output="$VARIANTS_DIR/tools-mcp-$name.json"
  "$RECORDER" - "$INPUTS_DIR/tools-mcp-unreachable.json" "$output" "$url" "$@" <<'PY'
import json
import sys

source, target, url, *flags = sys.argv[1:]
with open(source, encoding="utf-8") as handle:
    tools = json.load(handle)
server = next(tool for tool in tools if tool["type"] == "mcp")
server["server_url"] = url
if "eager" in flags:
    del server["defer_loading"]
if "no-match-filter" in flags:
    server["allowed_tools"] = ["no_such_tool"]
with open(target, "w", encoding="utf-8") as handle:
    json.dump(tools, handle, indent=2)
PY
  printf '%s' "$output"
}

# query_prompt ARGUMENTS_JSON
query_prompt() {
  printf 'This is a test of the tool loader. Call tool_search exactly once with exactly these arguments, unchanged: %s. Do not call any other tool and do not search again. Then reply with only the names of the tools that the search loaded, or "none".' "$1"
}

# Sets LEG_TURNS, LEG_PROMPTS, LEG_TOOLS, and LEG_ARGS for one named leg.
define_leg() {
  LEG_TURNS=1
  LEG_ARGS=(--tool-choice auto)
  case "$1" in
    explicit-server)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-explicit-server.json" ;;
    no-deferred)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-no-deferred.json" ;;
    defer-without-search)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-defer-without-search.json" ;;
    tool-choice-function)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mixed.json"
      LEG_ARGS=(--tool-choice '{"type":"function","name":"get_weather"}') ;;
    tool-choice-mcp)
      LEG_PROMPTS="$MCP_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mixed.json"
      LEG_ARGS=(--tool-choice '{"type":"mcp","server_label":"gitmcp_tiktoken"}') ;;
    tool-choice-required)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mixed.json"
      LEG_ARGS=(--tool-choice required) ;;
    mcp-deferred)
      LEG_PROMPTS="$MCP_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mcp-deferred.json" ;;
    mcp-no-description)
      LEG_PROMPTS="$MCP_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mcp-no-description.json" ;;
    mixed-catalog)
      LEG_TURNS=3 LEG_PROMPTS="$(printf '%s\n%s\n%s' "$MIXED_TURN1_PROMPT" "$MIXED_TURN2_PROMPT" "$MIXED_TURN3_PROMPT")"
      LEG_TOOLS="$INPUTS_DIR/tools-mixed.json"
      LEG_ARGS=(--tool-choice auto --tool-outputs "$INPUTS_DIR/tool_outputs.py") ;;
    mixed-parallel-off)
      LEG_TURNS=3 LEG_PROMPTS="$(printf '%s\n%s\n%s' "$MIXED_TURN1_PROMPT" "$MIXED_TURN2_PROMPT" "$MIXED_TURN3_PROMPT")"
      LEG_TOOLS="$INPUTS_DIR/tools-mixed.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --tool-outputs "$INPUTS_DIR/tool_outputs.py") ;;
    max-tool-calls)
      LEG_PROMPTS="$MAX_TOOL_CALLS_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mcp-deferred.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false --max-tool-calls 1) ;;
    continuation)
      LEG_TURNS=2 LEG_PROMPTS="$(printf '%s\n%s' "$MCP_PROMPT" "$FOLLOW_UP_PROMPT")"
      LEG_TOOLS="$INPUTS_DIR/tools-mcp-deferred.json" ;;
    query-function-description)
      LEG_PROMPTS="$(query_prompt '{"paths":["get_weather"],"query":"CURRENT"}')"
      LEG_TOOLS="$INPUTS_DIR/tools-query.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false) ;;
    repeat-same-response)
      LEG_PROMPTS="$REPEAT_SAME_RESPONSE_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-query.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false) ;;
    repeat-continuation)
      LEG_TURNS=2
      LEG_PROMPTS="$(printf '%s\n%s' "$(query_prompt '{"paths":["travel"],"query":"timezone"}')" \
        "$REPEAT_CONTINUATION_PROMPT")"
      LEG_TOOLS="$INPUTS_DIR/tools-query.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false) ;;
    mcp-unreachable-unused)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mcp-unreachable.json" ;;
    mcp-unreachable-used)
      LEG_PROMPTS="$BROKEN_MCP_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mcp-unreachable.json" ;;
    mcp-unreachable-eager)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$(broken_mcp_tools unreachable-eager https://mcp.invalid/mcp eager)" ;;
    failure-unreachable)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$INPUTS_DIR/tools-mcp-unreachable.json" ;;
    failure-unreachable-eager)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$(broken_mcp_tools unreachable-eager https://mcp.invalid/mcp eager)" ;;
    failure-http-500)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$(broken_mcp_tools http-500 https://httpbin.org/status/500)" ;;
    failure-http-500-eager)
      LEG_PROMPTS="$WEATHER_PROMPT"
      LEG_TOOLS="$(broken_mcp_tools http-500-eager https://httpbin.org/status/500 eager)" ;;
    failure-not-mcp)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$(broken_mcp_tools not-mcp https://example.com/mcp)" ;;
    failure-auth-required)
      LEG_PROMPTS="$WEATHER_PROMPT" LEG_TOOLS="$(broken_mcp_tools auth-required https://api.githubcopilot.com/mcp/)" ;;
    failure-empty-allowed)
      LEG_PROMPTS="$WEATHER_PROMPT"
      LEG_TOOLS="$(broken_mcp_tools empty-allowed https://gitmcp.io/openai/tiktoken no-match-filter)" ;;
    query-*)
      LEG_TOOLS="$INPUTS_DIR/tools-query.json"
      LEG_ARGS=(--tool-choice auto --parallel-tool-calls false)
      case "$1" in
        query-exact-name)        LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"get_timezone"}')" ;;
        query-name-fragment)     LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"timezone"}')" ;;
        query-description-word)  LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"latitude"}')" ;;
        query-synonym)           LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"airplane ticket"}')" ;;
        query-two-words)         LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"timezone distance"}')" ;;
        query-uppercase)         LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"GET_TIMEZONE"}')" ;;
        query-no-match)          LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"chocolate cake recipe"}')" ;;
        query-empty)             LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":""}')" ;;
        query-function-match)    LEG_PROMPTS="$(query_prompt '{"paths":["get_weather"],"query":"weather"}')" ;;
        query-function-mismatch) LEG_PROMPTS="$(query_prompt '{"paths":["get_weather"],"query":"timezone"}')" ;;
        query-outside-paths)     LEG_PROMPTS="$(query_prompt '{"paths":["get_weather"],"query":"get_exchange_rate"}')" ;;
        query-without-paths)     LEG_PROMPTS="$(query_prompt '{"query":"timezone"}')" ;;
        query-mcp)               LEG_PROMPTS="$(query_prompt '{"paths":["mcp_gitmcp_tiktoken"],"query":"code"}')" ;;
        query-partial-word)      LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"coord"}')" ;;
        query-plural)            LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"city"}')" ;;
        query-split-words)       LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"latitude city"}')" ;;
        query-space-for-underscore)
                                 LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"get timezone"}')" ;;
        query-description-phrase)
                                 LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"time zone"}')" ;;
        query-parameter-name)    LEG_PROMPTS="$(query_prompt '{"paths":["travel"],"query":"destination_airport"}')" ;;
        query-unknown-path)      LEG_PROMPTS="$(query_prompt '{"paths":["no_such_tool"]}')" ;;
        query-empty-paths)       LEG_PROMPTS="$(query_prompt '{"paths":[],"query":"timezone"}')" ;;
        *)
          echo "ERROR: unknown leg $1" >&2
          exit 1 ;;
      esac ;;
    replay)
      LEG_TURNS=2 LEG_PROMPTS="$(printf '%s\n%s' "$MCP_PROMPT" "$FOLLOW_UP_PROMPT")"
      LEG_TOOLS="$INPUTS_DIR/tools-mcp-deferred.json"
      LEG_ARGS=(--tool-choice auto --no-store --manual-item-replay) ;;
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
  [[ " $HOSTED_TOOL_SEARCH_SKIP_LEGS " == *" $1 "* ]]
}

selected() {
  [[ " $HOSTED_TOOL_SEARCH_GROUPS " == *" $1 "* ]]
}

# record_group ENDPOINT_FLAG ENDPOINT MODEL STREAM_FLAG OUTPUT LEG...
record_group() {
  local endpoint_flag="$1" endpoint="$2" model="$3" stream_flag="$4" output="$5"
  shift 5
  local staged leg
  staged="$(mktemp -u "$BASE_DIR/.hosted-tool-search-cassette.XXXXXX")"
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
  prefix="$BASE_DIR/hosted-tool-search"

  bold "═══ $provider_label ($endpoint) — model: $model ═══"

  if selected validation; then
    bold "validation (nonstreaming)"
    # shellcheck disable=SC2086 # VALIDATION_LEGS is a space-separated list of leg names
    record_group "$endpoint_flag" "$endpoint" "$model" --no-stream \
      "$prefix-validation-$suffix-$slug-nonstreaming.yaml" $VALIDATION_LEGS
  fi

  if selected contract; then
    local stream_flag mode
    for stream_flag in --stream --no-stream; do
      mode="streaming"
      [[ "$stream_flag" == "--no-stream" ]] && mode="nonstreaming"
      bold "contract ($mode)"
      # shellcheck disable=SC2086 # CONTRACT_LEGS is a space-separated list of leg names
      record_group "$endpoint_flag" "$endpoint" "$model" "$stream_flag" \
        "$prefix-contract-$suffix-$slug-$mode.yaml" $CONTRACT_LEGS
    done
  fi

  if selected query; then
    bold "query (nonstreaming)"
    # shellcheck disable=SC2086 # QUERY_LEGS is a space-separated list of leg names
    record_group "$endpoint_flag" "$endpoint" "$model" --no-stream \
      "$prefix-query-$suffix-$slug-nonstreaming.yaml" $QUERY_LEGS
  fi

  if selected edges; then
    bold "edges (nonstreaming)"
    # shellcheck disable=SC2086 # EDGE_LEGS is a space-separated list of leg names
    record_group "$endpoint_flag" "$endpoint" "$model" --no-stream \
      "$prefix-edges-$suffix-$slug-nonstreaming.yaml" $EDGE_LEGS
  fi

  if selected mcp-failures; then
    local failure_stream failure_mode
    for failure_stream in --stream --no-stream; do
      failure_mode="streaming"
      [[ "$failure_stream" == "--no-stream" ]] && failure_mode="nonstreaming"
      bold "mcp-failures ($failure_mode)"
      # shellcheck disable=SC2086 # MCP_FAILURE_LEGS is a space-separated list of leg names
      record_group "$endpoint_flag" "$endpoint" "$model" "$failure_stream" \
        "$prefix-mcp-failures-$suffix-$slug-$failure_mode.yaml" $MCP_FAILURE_LEGS
    done
  fi

  # The replayed transcript holds mcp_call items, which the gateway does not yet lower for vLLM.
  if selected replay && [[ "$endpoint_flag" == "--openai" ]]; then
    bold "replay (nonstreaming)"
    record_group "$endpoint_flag" "$endpoint" "$model" --no-stream \
      "$prefix-replay-$suffix-$slug-nonstreaming.yaml" replay
  fi

  if selected websocket; then
    bold "websocket (mcp-deferred leg)"
    local staged
    staged="$(mktemp -u "$BASE_DIR/.hosted-tool-search-cassette.XXXXXX")"
    define_leg mcp-deferred
    record_turns "$endpoint_flag" "$endpoint" "$model" "$LEG_TURNS" "$LEG_PROMPTS" "$LEG_TOOLS" \
      --stream "$staged" --transport websocket "${LEG_ARGS[@]}"
    finalize "$staged" "$prefix-websocket-$suffix-$slug.yaml"
  fi
}

mkdir -p "$BASE_DIR"

if [[ "$HOSTED_TOOL_SEARCH_RECORD_SET" == "openai" || "$HOSTED_TOOL_SEARCH_RECORD_SET" == "all" ]]; then
  record_provider_suite "OpenAI" --openai "https://api.openai.com" "$OPENAI_MODEL_NAME" "openai-reference"
fi

if [[ "$HOSTED_TOOL_SEARCH_RECORD_SET" == "gateway" || "$HOSTED_TOOL_SEARCH_RECORD_SET" == "all" ]]; then
  record_provider_suite "Gateway" --gateway "$GATEWAY_URL" "$GATEWAY_MODEL" "gateway"
fi

echo
green "hosted tool-search cassettes recorded -> $BASE_DIR"
