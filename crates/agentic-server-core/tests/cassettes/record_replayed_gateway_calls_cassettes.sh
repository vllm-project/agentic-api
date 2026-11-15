#!/usr/bin/env bash
# Records how a provider treats replayed gateway-executed call items (#492): mcp_call, web_search_call and
# code_interpreter_call sent back inline with store=false, and by id (the original stored, the replay with
# store=false). Every request goes through record_cassette.py; follow-up input is derived from the captured
# outputs, as in record_store_false_cassettes.sh. The fixtures are the existing ones: the gitmcp tiktoken server
# from hosted_tool_search/inputs (not deferred here), web_search/tools.json and code_interpreter/.
#
# One cassette per type and leg, in replayed_calls/ (or $REPLAYED_CALLS_CASSETTES_DIR):
#   replayed-calls-<type>-<leg>-<provider>-<model>-nonstreaming.yaml
#
#   inline      t1 store=false makes the call; t2 store=false replays t1's input and output items
#               (--manual-item-replay) with a follow-up question and tool_choice "none".
#   by-id       t1 store=true makes the call; t2 store=false sends t1's input message, its output items as returned
#               (with their ids) and the follow-up; t3 the same with every output item as an item_reference.
#   no-listing  mcp only: t2 of the inline leg without its mcp_list_tools item (tools declared, tool_choice "none").
#   no-tools    mcp and code-interpreter: t2 of the inline leg, declaring no tools (as a compaction summary does).
#
# A leg whose first turn has no item of its type is recorded as it came back and the dependent replays are
# skipped. Provider errors are kept as evidence; only an empty or unsanitized capture is discarded.
#
# Usage from the repository root:
#   OPENAI_API_KEY=... bash crates/agentic-server-core/tests/cassettes/record_replayed_gateway_calls_cassettes.sh
#   REPLAYED_CALLS_RECORD_SET=gateway GATEWAY_URL=http://127.0.0.1:9000 GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 \
#     bash crates/agentic-server-core/tests/cassettes/record_replayed_gateway_calls_cassettes.sh
#
# REPLAYED_CALLS_TYPES selects types (default "mcp web-search code-interpreter"); REPLAYED_CALLS_SKIP_LEGS omits legs
# ("<type>:<leg>" or "<leg>"). The gateway set needs a gateway that may reach gitmcp.io, a web-search provider and
# the embedded code interpreter; it records what that gateway does, including failures.

set -uo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="${REPLAYED_CALLS_CASSETTES_DIR:-$SCRIPTS_DIR/replayed_calls}"
PYTHON="${PYTHON:-python}"
RECORD_SET="${REPLAYED_CALLS_RECORD_SET:-openai}"
TYPES="${REPLAYED_CALLS_TYPES:-mcp web-search code-interpreter}"
SKIP_LEGS="${REPLAYED_CALLS_SKIP_LEGS:-}"
OPENAI_URL="${OPENAI_URL:-https://api.openai.com}"
OPENAI_MODEL_NAME="${OPENAI_MODEL:-gpt-5.6-sol}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
GATEWAY_MODEL="${GATEWAY_MODEL:-Qwen/Qwen3.6-35B-A3B-FP8}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-2048}"
PROXY_PORT="${PROXY_PORT:-7070}"

green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }
red()   { printf '\033[31m%s\033[0m\n' "$*"; }

case "$RECORD_SET" in
  openai)
    if [[ -z "${OPENAI_API_KEY:-}" ]]; then
      echo "ERROR: OPENAI_API_KEY must be set for REPLAYED_CALLS_RECORD_SET=openai" >&2
      exit 1
    fi
    BACKEND=(--openai "$OPENAI_URL")
    MODEL="$OPENAI_MODEL_NAME"
    PROVIDER=openai-reference
    ;;
  gateway)
    BACKEND=(--gateway "$GATEWAY_URL")
    MODEL="$GATEWAY_MODEL"
    PROVIDER=gateway
    ;;
  *)
    echo "ERROR: REPLAYED_CALLS_RECORD_SET must be openai or gateway" >&2
    exit 1
    ;;
esac
MODEL_SLUG="$(printf '%s' "$MODEL" | tr '/: ' '---')"

MCP_PROMPT='Use the tiktoken documentation tool to find which encoding gpt-4o uses. Answer in one sentence.'
MCP_FOLLOW_UP='Without calling a tool, answer from the documentation search result above only: which encoding did it report for gpt-4o?'
WEB_PROMPT='Use web search to search for the exact query "potato", then summarize the result in one sentence.'
WEB_FOLLOW_UP='Without calling a tool, answer from the search result above only: name one source it returned.'
CODE_FOLLOW_UP='Without calling a tool, answer from the code output above only: what exactly did the code print?'

WORK_DIR="$(mktemp -d)"
trap 'rm -rf -- "$WORK_DIR"' EXIT

# The gitmcp tiktoken server of the hosted tool-search fixtures, declared directly (no tool_search, not deferred).
"$PYTHON" - "$SCRIPTS_DIR/hosted_tool_search/inputs/tools-mcp-deferred.json" "$WORK_DIR/tools-mcp.json" <<'PY'
import json
import sys

tools = json.load(open(sys.argv[1], encoding="utf-8"))
server = next(tool for tool in tools if tool["type"] == "mcp")
server.pop("defer_loading", None)
server.pop("server_description", None)
json.dump([server], open(sys.argv[2], "w", encoding="utf-8"), indent=2)
PY
printf '["auto", "none"]' > "$WORK_DIR/auto-none.json"
printf '["required", "none"]' > "$WORK_DIR/required-none.json"

# define_type TYPE: sets ITEM_TYPE, TOOLS, PROMPT, FOLLOW_UP, FIRST_CHOICE, SEQUENCE, EXTRA_ARGS and EXTRA_LEGS.
define_type() {
  EXTRA_ARGS=()
  case "$1" in
    mcp)
      ITEM_TYPE=mcp_call TOOLS="$WORK_DIR/tools-mcp.json" PROMPT="$MCP_PROMPT" FOLLOW_UP="$MCP_FOLLOW_UP"
      FIRST_CHOICE=auto SEQUENCE="$WORK_DIR/auto-none.json" EXTRA_LEGS="no-listing no-tools" ;;
    web-search)
      ITEM_TYPE=web_search_call TOOLS="$SCRIPTS_DIR/web_search/tools.json" PROMPT="$WEB_PROMPT"
      FOLLOW_UP="$WEB_FOLLOW_UP" FIRST_CHOICE=auto SEQUENCE="$WORK_DIR/auto-none.json" EXTRA_LEGS="" ;;
    code-interpreter)
      ITEM_TYPE=code_interpreter_call PROMPT="$(cat "$SCRIPTS_DIR/code_interpreter/prompts.txt")"
      FOLLOW_UP="$CODE_FOLLOW_UP" FIRST_CHOICE=required SEQUENCE="$WORK_DIR/required-none.json"
      EXTRA_ARGS=(--parallel-tool-calls false) EXTRA_LEGS="no-tools"
      if [[ "$PROVIDER" == gateway ]]; then
        TOOLS="$SCRIPTS_DIR/code_interpreter/gateway_tools.json"
      else
        TOOLS="$SCRIPTS_DIR/code_interpreter/openai_tools.json"
      fi ;;
    *)
      echo "ERROR: unknown type $1 (mcp, web-search, code-interpreter)" >&2
      exit 1 ;;
  esac
}

skipped() {
  [[ " $SKIP_LEGS " == *" $2 "* || " $SKIP_LEGS " == *" $1:$2 "* ]]
}

assert_sanitized() {
  CASSETTE_PATH="$1" "$PYTHON" -c '
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

# replay_input CASSETTE TURN MODE TYPE FOLLOW_UP: prints the next request's input, built from turn TURN (1-based):
# its input message, its output items (MODE full: as returned; ref: as item_reference; no-listing: without
# mcp_list_tools) and the follow-up. Fails when that turn did not complete with an item of TYPE.
replay_input() {
  "$PYTHON" - "$@" <<'PY'
import json
import sys

import yaml

path, turn, mode, item_type, follow_up = sys.argv[1:]
turns = yaml.safe_load(open(path, encoding="utf-8"))["turns"]
record = turns[int(turn) - 1]
response = record["response"].get("body")
if record["response"].get("status_code") != 200 or not isinstance(response, dict):
    sys.exit(f"turn {turn} did not succeed (HTTP {record['response'].get('status_code')})")
output = response.get("output") or []
if response.get("status") != "completed" or not any(item.get("type") == item_type for item in output):
    sys.exit(f"turn {turn} has no completed {item_type}: {[item.get('type') for item in output]}")
sent = record["request"]["body"]["input"]
if isinstance(sent, str):
    sent = [{"type": "message", "role": "user", "content": sent}]
user = [item for item in sent if item.get("type", "message") == "message" and item.get("role") == "user"][:1]
if mode == "full":
    replayed = output
elif mode == "ref":
    replayed = [{"type": "item_reference", "id": item["id"]} for item in output]
elif mode == "no-listing":
    replayed = [item for item in output if item.get("type") != "mcp_list_tools"]
else:
    replayed = output
print(json.dumps(user + replayed + [{"type": "message", "role": "user", "content": follow_up}]))
PY
}

# record STAGED [RECORDER ARGS...]: one record_cassette.py run appended to STAGED.
record() {
  local staged="$1"
  shift
  local append_flag=()
  [[ -s "$staged" ]] && append_flag=(--append)
  "$PYTHON" "$SCRIPTS_DIR/record_cassette.py" \
    --mode responses \
    --no-stream \
    --model "$MODEL" \
    "${BACKEND[@]}" \
    --max-output-tokens "$MAX_OUTPUT_TOKENS" \
    --proxy-port "$PROXY_PORT" \
    --output "$staged" \
    ${append_flag[@]+"${append_flag[@]}"} \
    "$@"
}

leg_path() {
  printf '%s/replayed-calls-%s-%s-%s-%s-nonstreaming.yaml' "$BASE_DIR" "$1" "$2" "$PROVIDER" "$MODEL_SLUG"
}

record_type() {
  local type="$1" staged inline_staged
  define_type "$type"
  bold "═══ $type ($ITEM_TYPE) — $PROVIDER, model $MODEL ═══"

  inline_staged=""
  if skipped "$type" inline && [[ -z "$EXTRA_LEGS" ]]; then
    bold "  inline (skipped)"
  else
    bold "  inline"
    inline_staged="$(mktemp "$BASE_DIR/.replayed-calls.XXXXXX")"
    : > "$inline_staged"
    printf '%s\n%s\n' "$PROMPT" "$FOLLOW_UP" | record "$inline_staged" --turns 2 --no-store --manual-item-replay \
      --tools "$TOOLS" --tool-choice-sequence "$SEQUENCE" ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
      || red "  inline: the recorder stopped early (kept as evidence)"
  fi

  local leg mode input
  for leg in $EXTRA_LEGS; do
    if skipped "$type" "$leg"; then
      bold "  $leg (skipped)"
      continue
    fi
    bold "  $leg"
    if [[ -z "$inline_staged" ]] || ! input="$(replay_input "$inline_staged" 1 \
        "$([[ $leg == no-listing ]] && echo no-listing || echo full)" "$ITEM_TYPE" "$FOLLOW_UP")"; then
      red "  $leg: no inline call to replay; skipped"
      continue
    fi
    printf '%s' "$input" > "$WORK_DIR/$type-$leg.json"
    staged="$(mktemp "$BASE_DIR/.replayed-calls.XXXXXX")"
    : > "$staged"
    if [[ "$leg" == no-tools ]]; then
      record "$staged" --turns 1 --no-store --input-file "$WORK_DIR/$type-$leg.json" ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} < /dev/null \
        || red "  $leg: the provider refused it (kept as evidence)"
    else
      record "$staged" --turns 1 --no-store --input-file "$WORK_DIR/$type-$leg.json" --tools "$TOOLS" \
        --tool-choice none ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} < /dev/null \
        || red "  $leg: the provider refused it (kept as evidence)"
    fi
    finalize "$staged" "$(leg_path "$type" "$leg")"
  done

  if [[ -n "$inline_staged" ]]; then
    if skipped "$type" inline; then
      rm -f -- "$inline_staged"
    else
      finalize "$inline_staged" "$(leg_path "$type" inline)"
    fi
  fi

  if skipped "$type" by-id; then
    bold "  by-id (skipped)"
    return 0
  fi
  bold "  by-id"
  staged="$(mktemp "$BASE_DIR/.replayed-calls.XXXXXX")"
  : > "$staged"
  printf '%s\n' "$PROMPT" | record "$staged" --turns 1 --tools "$TOOLS" --tool-choice "$FIRST_CHOICE" ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
    || red "  by-id: the stored turn failed (kept as evidence)"
  for mode in full ref; do
    if ! input="$(replay_input "$staged" 1 "$mode" "$ITEM_TYPE" "$FOLLOW_UP")"; then
      red "  by-id ($mode): nothing to replay; skipped"
      continue
    fi
    printf '%s' "$input" > "$WORK_DIR/$type-by-id-$mode.json"
    record "$staged" --turns 1 --no-store --input-file "$WORK_DIR/$type-by-id-$mode.json" --tools "$TOOLS" \
      --tool-choice none ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} < /dev/null \
      || red "  by-id ($mode): the provider refused it (kept as evidence)"
  done
  finalize "$staged" "$(leg_path "$type" by-id)"
}

mkdir -p "$BASE_DIR"
for type in $TYPES; do
  record_type "$type"
done
echo
green "replayed-call cassettes -> $BASE_DIR"
