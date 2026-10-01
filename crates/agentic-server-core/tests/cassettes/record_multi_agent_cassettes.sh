#!/usr/bin/env bash
# Record guide-derived multi-agent scenarios through record_cassette.py.
# Guide: https://developers.openai.com/api/docs/guides/responses-multi-agent
# These are client drivers; hosted collaboration is executed by the service.
set -euo pipefail

usage() {
  cat <<'HELP'
Usage: bash record_multi_agent_cassettes.sh [--dry-run | --help]

Record OpenAI first, review the observations, then implement and compare gateway behavior.
Records delegated review, proposal comparison, mixed web/MCP/shell tools,
client-owned tool search, functions, and custom tools, and two-agent algorithm jobs,
each over HTTP JSON/SSE or a persistent WebSocket session.
Set MULTI_AGENT_TRANSPORT=websocket for five separate duplex YAMLs per provider.
The same prompts and tool fixtures are reused unchanged; existing HTTP YAMLs are untouched.
WebSocket captures retain actual client/server frames, handshake headers (credentials
masked), injection outcomes, and close/failure details without synthesizing SSE.
Function, shell, custom, and discovery outputs are submitted for characterization;
a provider may reject an injection kind. Such failures are preserved and reported,
not retried as though accepted input were rejected.
Positive scenarios enable multi-agent with store:true and the multi-agent beta header.
Select MULTI_AGENT_SUITE=websocket-edge-cases separately for four state/race/atomicity probes.
All edge cases share one YAML per provider, with a labeled session for each probe.
MULTI_AGENT_EDGE_ACTIVE=true uses [websocket-active] sibling text generation during injection
and writes a separate ws-active-text-edge-cases YAML. Reruns keep completed cases and append new attempts.
MULTI_AGENT_EDGE_CASE selects one probe (default: all); MULTI_AGENT_EDGE_TIMEOUT bounds each probe (default: 120 seconds).
Client-tool continuations use previous_response_id and matching tool outputs.

Environment:
  MULTI_AGENT_RECORD_SET  openai (default), gateway, or all
  MULTI_AGENT_SUITE       all (default), review, proposals, mixed-tools, client-owned-tools, code-interpreter, or websocket-edge-cases
  MULTI_AGENT_TRANSPORT   http (default) or websocket
  MULTI_AGENT_STREAM_MODE both (default), streaming, or nonstreaming
  MULTI_AGENT_MAX_CONTINUATIONS  Extra client-tool requests per scenario (default: 10; max: 100)
  HTTP_READ_TIMEOUT      Upstream read inactivity timeout in seconds (default: 900)
  OPENAI_API_KEY         Required for live OpenAI recording; never written to logs
  OPENAI_MODEL           Default: gpt-5.6-sol
  GATEWAY_URL            Default: http://localhost:9000
  GATEWAY_MODEL          Served gateway model (also accepts MODEL)
  MULTI_AGENT_OUTPUT_DIR Cassette directory (default: multi_agent beside this script)
  MAX_CONCURRENT_SUBAGENTS Maximum active subagents, excluding root (default: 3; code-interpreter: 2)
  MAX_OUTPUT_TOKENS     Default: 16384; 0 omits the request limit
  PROXY_PORT            Default: 7070
  PYTHON                Python executable with recorder dependencies

Examples (from the repository root, with OPENAI_API_KEY already exported):
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
  MULTI_AGENT_RECORD_SET=gateway GATEWAY_MODEL=your-served-model \
    bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh

To supply dependencies without changing your project environment:
  uv run --no-project --with click --with fastapi --with httpx --with uvicorn \
    --with pyyaml bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh

--dry-run prints commands without contacting APIs or writing any files.
all selects review, proposals, mixed-tools, client-owned-tools, and code-interpreter:
ten YAML files per provider with the default both mode, or five for one stream mode.
The former parameter-only edge-cases, failure-cases, and compaction recordings
are not behavioral coverage and are no longer generated. Existing YAML is left alone.
Runtime edge cases, failures, and compaction still need dedicated scenarios.
Recordings use stable filenames containing provider, group/scenario, model, and mode.
Re-running replaces those cassettes. A failed recording is retained for inspection.
Failures do not skip other selected recordings; the script exits nonzero after listing failures.
All scenario prompts are sections of multi_agent/prompts.txt; no scripts or run directories are copied.
The client-tool scenarios continue pending client calls within the configured limit; inspect pending calls and completion
after recording. Capture does not enforce agent counts or a particular answer.

mixed-tools is included in all and can be selected alone. One prompt asks
three agents to use web search, GitMCP tiktoken, and the local shell. Its tool
configuration contains only web_search, mcp, and shell. The shell fixture supplies
stdout 55 followed by a newline, empty stderr, and exit code 0 for the exact command.
Unsupported commands receive simulated failures; commands are not executed.
The MCP fixture uses https://gitmcp.io/openai/tiktoken and allows
search_tiktoken_documentation without an approval round trip.

client-owned-tools is included in all and can be selected alone. It records only
the [client-owned-tools] section of prompts.txt in a fresh session, with the standalone tool_search
catalog plus the custom echo tool and their output fixtures. It exposes no web,
MCP, or shell tools and writes
separate client-owned-tools YAML files. Select MULTI_AGENT_SUITE=client-owned-tools to run it.

code-interpreter asks two subagents to find popular LeetCode problems online using
web search, then implement and execute two algorithms each using code interpreter.
Both tools are declared with parallel_tool_calls:true.
It uses two subagent slots by default. Execution outputs come from the service;
no client output fixtures or automatic client-tool continuations are used.
Gateway recording requires a binary built with --features embedded-code-interpreter
and a ready, operator-enabled runtime; see docs/design/embedded-code-interpreter.md.

Remaining coverage: partial/duplicate/mismatched outputs for a real pending call,
branch/configuration changes, long-context compaction and its races, all hosted
actions, WebSocket, and gateway dependency replay. Passing this suite does not
complete MA-01 characterization. Gateway multi-agent support is scoped to store:true.
HELP
}

DRY_RUN=false
case "${1:-}" in
  --help|-h) usage; exit 0 ;;
  --dry-run) DRY_RUN=true ;;
  "") ;;
  *) usage >&2; exit 2 ;;
esac
if (( $# > 1 )); then usage >&2; exit 2; fi

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECORDER="$SCRIPTS_DIR/record_cassette.py"
RECORD_SET="${MULTI_AGENT_RECORD_SET:-openai}"
SUITE="${MULTI_AGENT_SUITE:-all}"
STREAM_MODE="${MULTI_AGENT_STREAM_MODE:-both}"
TRANSPORT="${MULTI_AGENT_TRANSPORT:-http}"
HTTP_READ_TIMEOUT="${HTTP_READ_TIMEOUT:-900}"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-5.6-sol}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
GATEWAY_MODEL="${GATEWAY_MODEL:-${MODEL:-}}"
FIXTURES_DIR="$SCRIPTS_DIR/multi_agent"
TOOL_SEARCH_FIXTURES="$SCRIPTS_DIR/tool_search"
BASE_DIR="${MULTI_AGENT_OUTPUT_DIR:-$FIXTURES_DIR}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-16384}"
PROXY_PORT="${PROXY_PORT:-7070}"
PYTHON="${PYTHON:-python3}"
MAX_CONCURRENT_SUBAGENTS="${MAX_CONCURRENT_SUBAGENTS:-}"
if [[ -n "$MAX_CONCURRENT_SUBAGENTS" && ! "$MAX_CONCURRENT_SUBAGENTS" =~ ^[1-9][0-9]*$ ]]; then
  echo 'ERROR: MAX_CONCURRENT_SUBAGENTS must be a positive integer' >&2
  exit 2
fi

case "$SUITE" in
  all|review|proposals|mixed-tools|client-owned-tools|code-interpreter|websocket-edge-cases) ;;
  *) echo 'ERROR: MULTI_AGENT_SUITE must be all, review, proposals, mixed-tools, client-owned-tools, code-interpreter, or websocket-edge-cases' >&2; exit 2 ;;
esac
if [[ ( "$SUITE" == all || "$SUITE" == code-interpreter ) && -n "$MAX_CONCURRENT_SUBAGENTS" ]] &&
   (( MAX_CONCURRENT_SUBAGENTS < 2 )); then
  echo 'ERROR: code-interpreter requires MAX_CONCURRENT_SUBAGENTS >= 2; omit it to use per-scenario defaults' >&2
  exit 2
fi
case "$TRANSPORT" in
  http|websocket) ;;
  *) echo 'ERROR: MULTI_AGENT_TRANSPORT must be http or websocket' >&2; exit 2 ;;
esac
if [[ "$SUITE" == websocket-edge-cases && "$TRANSPORT" != websocket ]]; then
  echo 'ERROR: websocket-edge-cases requires MULTI_AGENT_TRANSPORT=websocket' >&2
  exit 2
fi
case "$STREAM_MODE" in
  both|streaming|nonstreaming) ;;
  *) echo 'ERROR: MULTI_AGENT_STREAM_MODE must be both, streaming, or nonstreaming' >&2; exit 2 ;;
esac
if [[ ! "$HTTP_READ_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
  echo 'ERROR: HTTP_READ_TIMEOUT must be a positive integer in seconds' >&2
  exit 2
fi

case "$RECORD_SET" in
  openai|gateway|all) ;;
  *) echo 'ERROR: MULTI_AGENT_RECORD_SET must be openai, gateway, or all' >&2; exit 2 ;;
esac
if [[ "$RECORD_SET" != openai && -z "$GATEWAY_MODEL" ]]; then
  echo 'ERROR: GATEWAY_MODEL is required for gateway recordings' >&2
  exit 2
fi
if [[ "$DRY_RUN" == false && "$RECORD_SET" != gateway && -z "${OPENAI_API_KEY:-}" ]]; then
  echo 'ERROR: export OPENAI_API_KEY before recording OpenAI references' >&2
  exit 2
fi
if [[ ! "$MAX_OUTPUT_TOKENS" =~ ^[0-9]+$ || ! "$PROXY_PORT" =~ ^[0-9]+$ ]]; then
  echo 'ERROR: MAX_OUTPUT_TOKENS and PROXY_PORT must be nonnegative integers' >&2
  exit 2
fi
if [[ "$DRY_RUN" == false ]]; then
  "$PYTHON" -c 'import click, fastapi, httpx, uvicorn, yaml' || {
    echo 'ERROR: recorder dependencies are missing; see --help for the uv command' >&2
    exit 2
  }
fi

for fixture in prompts.txt tools.json tool_outputs.py mixed_tools.json mixed_tool_outputs.py client_owned_tools.json code_interpreter_tools.json; do
  if [[ ! -f "$FIXTURES_DIR/$fixture" ]]; then
    echo "ERROR: missing multi-agent fixture: $FIXTURES_DIR/$fixture" >&2
    exit 2
  fi
done
if [[ "$SUITE" == all || "$SUITE" == client-owned-tools ]]; then
  for fixture in returned_tools.json function_outputs.json; do
    if [[ ! -f "$TOOL_SEARCH_FIXTURES/$fixture" ]]; then
      echo "ERROR: missing tool-search fixture: $TOOL_SEARCH_FIXTURES/$fixture" >&2
      exit 2
    fi
  done
fi
if [[ "$DRY_RUN" == false ]]; then
  mkdir -p "$BASE_DIR"
fi

if [[ ! -f "$SCRIPTS_DIR/custom_tool/tool_outputs.json" ]]; then
  echo 'ERROR: missing shared custom-tool output fixture' >&2
  exit 2
fi

FAILED_RECORDINGS=()

scenario_prompts() {
  awk -v section="[$1]" -v expected="$2" '
    /^\[/ { active = ($0 == section); next }
    active && NF { print; count++ }
    END { if (count != expected) exit 1 }
  ' "$FIXTURES_DIR/prompts.txt"
}

record_scenarios() {
  local provider="$1" endpoint_flag="$2" endpoint="$3" model="$4"
  if [[ "$SUITE" == websocket-edge-cases ]]; then
    local -a edge_command=("$PYTHON" -u "$SCRIPTS_DIR/websocket_recorder.py"
      --provider "$provider" --url "$endpoint" --model "$model" --output-dir "$BASE_DIR"
      --case "${MULTI_AGENT_EDGE_CASE:-all}" --timeout "${MULTI_AGENT_EDGE_TIMEOUT:-120}")
    case "${MULTI_AGENT_EDGE_ACTIVE:-false}" in
      true) edge_command+=(--active) ;;
      false) ;;
      *) echo 'ERROR: MULTI_AGENT_EDGE_ACTIVE must be true or false' >&2; exit 2 ;;
    esac
    if [[ "$DRY_RUN" == true ]]; then edge_command+=(--dry-run); fi
    if "${edge_command[@]}"; then :; else
      local edge_status=$?
      if (( edge_status == 130 || edge_status == 143 )); then exit "$edge_status"; fi
      FAILED_RECORDINGS+=("$provider websocket edge cases")
    fi
    return
  fi
  local scenario mode turns output prompts model_slug status agent_limit multi_agent_config
  model_slug="$(printf '%s' "$model" | tr '/: ' '---')"
  local -a command tool_args modes
  modes=(nonstreaming streaming)
  if [[ "$TRANSPORT" == websocket ]]; then modes=(websocket); fi
  for scenario in review proposals mixed-tools client-owned-tools code-interpreter; do
    if [[ "$SUITE" != all && "$scenario" != "$SUITE" ]]; then continue; fi
    turns=1
    agent_limit="${MAX_CONCURRENT_SUBAGENTS:-3}"
    if [[ "$scenario" == code-interpreter ]]; then agent_limit="${MAX_CONCURRENT_SUBAGENTS:-2}"; fi
    multi_agent_config="$(printf '{"enabled":true,"max_concurrent_subagents":%s}' "$agent_limit")"
    tool_args=()
    if [[ "$scenario" == proposals ]]; then
      tool_args=(--auto-tool-continuations "${MULTI_AGENT_MAX_CONTINUATIONS:-10}" --tools "$FIXTURES_DIR/tools.json" --tool-outputs "$FIXTURES_DIR/tool_outputs.py")
    elif [[ "$scenario" == mixed-tools ]]; then
      tool_args=(--auto-tool-continuations "${MULTI_AGENT_MAX_CONTINUATIONS:-10}"
        --tools "$FIXTURES_DIR/mixed_tools.json"
        --tool-outputs "$FIXTURES_DIR/mixed_tool_outputs.py")
    elif [[ "$scenario" == code-interpreter ]]; then
      tool_args=(--tools "$FIXTURES_DIR/code_interpreter_tools.json" --parallel-tool-calls true)
    elif [[ "$scenario" == client-owned-tools ]]; then
      tool_args=(--auto-tool-continuations "${MULTI_AGENT_MAX_CONTINUATIONS:-10}"
        --tools "$FIXTURES_DIR/client_owned_tools.json"
        --tool-search-output-tools "$TOOL_SEARCH_FIXTURES/returned_tools.json"
        --tool-outputs "$FIXTURES_DIR/mixed_tool_outputs.py")
    fi
    if ! prompts="$(scenario_prompts "$scenario" "$turns")"; then
      echo "ERROR: prompts.txt must contain $turns prompt(s) in [$scenario]" >&2
      exit 2
    fi
    for mode in "${modes[@]}"; do
      if [[ "$TRANSPORT" == http && "$STREAM_MODE" != both && "$mode" != "$STREAM_MODE" ]]; then continue; fi
      output="$BASE_DIR/multi-agent-$provider-$scenario-$model_slug-$mode.yaml"
      command=("$PYTHON" -u "$RECORDER" --mode responses --transport "$TRANSPORT" --turns "$turns"
        --model "$model" "$endpoint_flag" "$endpoint" --proxy-port "$PROXY_PORT"
        --multi-agent "$multi_agent_config" --openai-beta responses_multi_agent=v1
        --http-read-timeout "$HTTP_READ_TIMEOUT"
        --max-output-tokens "$MAX_OUTPUT_TOKENS" --output "$output" "${tool_args[@]}")
      if [[ "$mode" != nonstreaming ]]; then command+=(--stream); else command+=(--no-stream); fi
      if [[ "$DRY_RUN" == true ]]; then
        printf '%q ' "${command[@]}"
        printf '<<< %q\n' "$prompts"
        continue
      fi
      printf 'Recording %s %s %s with %s\n' "$provider" "$scenario" "$mode" "$model"
      if "${command[@]}" <<< "$prompts"; then
        echo "Captured $output; agent behavior and completion are evaluated after recording."
      else
        status=$?
        # Respect cancellation instead of starting another API request.
        if (( status == 130 || status == 143 )); then exit "$status"; fi
        if [[ -f "$output" ]]; then
          echo "ERROR: recording failed; inspect YAML at $output for captured exchanges" >&2
        else
          echo "ERROR: recording failed before a cassette was created: $output" >&2
        fi
        FAILED_RECORDINGS+=("$output")
      fi
    done
  done
}

if [[ "$RECORD_SET" == openai || "$RECORD_SET" == all ]]; then
  record_scenarios openai-reference --openai https://api.openai.com "$OPENAI_MODEL"
fi
if [[ "$RECORD_SET" == gateway || "$RECORD_SET" == all ]]; then
  record_scenarios gateway --gateway "$GATEWAY_URL" "$GATEWAY_MODEL"
fi
if (( ${#FAILED_RECORDINGS[@]} > 0 )); then
  echo 'ERROR: selected recordings were attempted, but these recordings failed:' >&2
  printf '  %s\n' "${FAILED_RECORDINGS[@]}" >&2
  exit 1
fi
if [[ "$DRY_RUN" == true ]]; then
  echo 'Dry run complete; no API requests were sent.'
else
  echo "Selected $TRANSPORT observations captured. Inspect completion and errors before claiming parity."
fi
