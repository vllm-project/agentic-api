#!/usr/bin/env bash
# Record guide-derived HTTP multi-agent scenarios through record_cassette.py.
# Guide: https://developers.openai.com/api/docs/guides/responses-multi-agent
# These are client drivers; hosted collaboration is executed by the service.
set -euo pipefail

usage() {
  cat <<'HELP'
Usage: bash record_multi_agent_cassettes.sh [--dry-run | --help]

Record OpenAI first, review the observations, then implement and compare gateway behavior.
Records delegated review (one request), proposal comparison (bounded continuations), and
three separate web/MCP/shell tasks (bounded continuations), each over HTTP JSON and SSE.
All scenarios enable multi-agent with store:true and the multi-agent beta header.
Proposal and mixed-tools continuations use previous_response_id and matching tool outputs.

Environment:
  MULTI_AGENT_RECORD_SET  openai (default), gateway, or all
  MULTI_AGENT_SUITE       all (default), workflows, review, proposals, or mixed-tools
  MULTI_AGENT_STREAM_MODE both (default), streaming, or nonstreaming
  MULTI_AGENT_MAX_CONTINUATIONS  Extra client-tool requests per scenario (default: 10; max: 100)
  HTTP_READ_TIMEOUT      Upstream read inactivity timeout in seconds (default: 300)
  OPENAI_API_KEY         Required for live OpenAI recording; never written to logs
  OPENAI_MODEL           Default: gpt-5.6-sol
  GATEWAY_URL            Default: http://localhost:9000
  GATEWAY_MODEL          Served gateway model (also accepts MODEL)
  MULTI_AGENT_OUTPUT_DIR Cassette directory (default: multi_agent beside this script)
  MAX_CONCURRENT_SUBAGENTS Maximum active subagents, excluding root (default: 3)
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
all/workflows selects review, proposals, and mixed-tools: six YAML files per provider
with the default both mode, or three when selecting one stream mode.
The former parameter-only edge-cases, failure-cases, and compaction recordings
are not behavioral coverage and are no longer generated. Existing YAML is left alone.
Runtime edge cases, failures, and compaction still need dedicated scenarios.
Recordings use stable filenames containing provider, group/scenario, model, and mode.
Re-running replaces those cassettes. A failed recording is retained for inspection.
Scenario inputs are fixed fixture files; no scripts or run directories are copied.
The proposal and mixed-tools drivers continue pending client calls within the configured limit; inspect pending calls and completion
after recording. Capture does not enforce agent counts or a particular answer.

mixed-tools is included in all/workflows and can be selected alone. It records one initial
prompt assigning web search, GitMCP tiktoken, and local-shell tasks to separate agents,
once as JSON and once as SSE. Like record_shell_cassettes.sh, the second request
submits simulated shell_call_output for the exact requested fixture command.
The output is 55 followed by a newline, empty stderr, and exit code 0.
Model-generated commands are not executed; unsupported commands stop the driver.
The MCP fixture uses https://gitmcp.io/openai/tiktoken
and allows search_tiktoken_documentation without an approval round trip.
This records the selected model's actual support for the combined configuration.

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
HTTP_READ_TIMEOUT="${HTTP_READ_TIMEOUT:-300}"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-5.6-sol}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
GATEWAY_MODEL="${GATEWAY_MODEL:-${MODEL:-}}"
FIXTURES_DIR="$SCRIPTS_DIR/multi_agent"
BASE_DIR="${MULTI_AGENT_OUTPUT_DIR:-$FIXTURES_DIR}"
MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-16384}"
PROXY_PORT="${PROXY_PORT:-7070}"
PYTHON="${PYTHON:-python3}"
MAX_CONCURRENT_SUBAGENTS="${MAX_CONCURRENT_SUBAGENTS:-3}"
if [[ ! "$MAX_CONCURRENT_SUBAGENTS" =~ ^[1-9][0-9]*$ ]]; then
  echo 'ERROR: MAX_CONCURRENT_SUBAGENTS must be a positive integer' >&2
  exit 2
fi
MULTI_AGENT_CONFIG="$(printf '{"enabled":true,"max_concurrent_subagents":%s}' "$MAX_CONCURRENT_SUBAGENTS")"

case "$SUITE" in
  all|workflows|review|proposals|mixed-tools) ;;
  *) echo 'ERROR: MULTI_AGENT_SUITE must be all, workflows, review, proposals, or mixed-tools' >&2; exit 2 ;;
esac
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

for fixture in review.txt proposals.txt tools.json tool_outputs.py mixed-tools.txt mixed_tools.json mixed_tool_outputs.py; do
  if [[ ! -f "$FIXTURES_DIR/$fixture" ]]; then
    echo "ERROR: missing multi-agent fixture: $FIXTURES_DIR/$fixture" >&2
    exit 2
  fi
done
if [[ "$DRY_RUN" == false ]]; then
  mkdir -p "$BASE_DIR"
fi

record_workflows() {
  local provider="$1" endpoint_flag="$2" endpoint="$3" model="$4"
  local scenario mode turns output prompts model_slug
  model_slug="$(printf '%s' "$model" | tr '/: ' '---')"
  local -a command tool_args
  for scenario in review proposals mixed-tools; do
    if [[ "$SUITE" == review || "$SUITE" == proposals || "$SUITE" == mixed-tools ]]; then
      if [[ "$scenario" != "$SUITE" ]]; then continue; fi
    fi
    turns=1
    tool_args=()
    if [[ "$scenario" == proposals ]]; then
      tool_args=(--auto-tool-continuations "${MULTI_AGENT_MAX_CONTINUATIONS:-10}" --tools "$FIXTURES_DIR/tools.json" --tool-outputs "$FIXTURES_DIR/tool_outputs.py")
    elif [[ "$scenario" == mixed-tools ]]; then
      tool_args=(--auto-tool-continuations "${MULTI_AGENT_MAX_CONTINUATIONS:-10}" --tools "$FIXTURES_DIR/mixed_tools.json" --tool-outputs "$FIXTURES_DIR/mixed_tool_outputs.py")
    fi
    prompts="$FIXTURES_DIR/$scenario.txt"
    for mode in nonstreaming streaming; do
      if [[ "$STREAM_MODE" != both && "$mode" != "$STREAM_MODE" ]]; then continue; fi
      output="$BASE_DIR/multi-agent-$provider-$scenario-$model_slug-$mode.yaml"
      command=("$PYTHON" -u "$RECORDER" --mode responses --transport http --turns "$turns"
        --model "$model" "$endpoint_flag" "$endpoint" --proxy-port "$PROXY_PORT"
        --multi-agent "$MULTI_AGENT_CONFIG" --openai-beta responses_multi_agent=v1
        --http-read-timeout "$HTTP_READ_TIMEOUT"
        --max-output-tokens "$MAX_OUTPUT_TOKENS" --output "$output" "${tool_args[@]}")
      if [[ "$mode" == streaming ]]; then command+=(--stream); else command+=(--no-stream); fi
      if [[ "$DRY_RUN" == true ]]; then
        printf '%q ' "${command[@]}"
        printf '< %q\n' "$prompts"
        continue
      fi
      printf 'Recording %s %s %s with %s\n' "$provider" "$scenario" "$mode" "$model"
      if ! "${command[@]}" < "$prompts"; then
        echo "ERROR: recording failed; captured YAML retained at $output" >&2
        return 1
      fi
      echo "Captured $output; agent behavior and completion are evaluated after recording."
    done
  done
}

if [[ "$RECORD_SET" == openai || "$RECORD_SET" == all ]]; then
  record_workflows openai-reference --openai https://api.openai.com "$OPENAI_MODEL"
fi
if [[ "$RECORD_SET" == gateway || "$RECORD_SET" == all ]]; then
  record_workflows gateway --gateway "$GATEWAY_URL" "$GATEWAY_MODEL"
fi
if [[ "$DRY_RUN" == true ]]; then
  echo 'Dry run complete; no API requests were sent.'
else
  echo 'Selected HTTP observations captured. Review OpenAI cassettes before implementing gateway behavior.'
fi
