#!/usr/bin/env bash
# Record OpenAI or gateway steering through the existing duplex WebSocket recorder.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PYTHONDONTWRITEBYTECODE=1
PYTHON="${PYTHON:-python3}"
SUITE="${STEERING_SUITE:-smoke}"
CASE="${STEERING_CASE:-}"
RECORD_SET="${STEERING_RECORD_SET:-openai}"
MODEL_OVERRIDE=""
GATEWAY_BASE="${GATEWAY_URL:-http://localhost:9000}"
UNSUPPORTED_MODEL="${STEERING_UNSUPPORTED_MODEL:-gpt-5.6}"
OUTPUT_DIR="${STEERING_OUTPUT_DIR:-$SCRIPT_DIR/multi_turn_steering}"
REPEAT="${STEERING_REPEAT:-1}"
TIMEOUT="${STEERING_TIMEOUT:-300}"
QUIET="${STEERING_QUIET_SECONDS:-3}"
BURST="${STEERING_BURST_COUNT:-32}"
MAX_TOKENS="${MAX_OUTPUT_TOKENS:-8192}"
DRY_RUN=false
LIST_CASES=false

usage() {
  cat <<'HELP'
Usage: bash record_multi_turn_steering_cassettes.sh [options]

Steering uses only websocket_recorder.py steering. Six categories produce six
YAML files per provider under multi_turn_steering/:
  mixed-tools-steer     Original single-agent mixed tools with two steering updates
  client-tools-steer    Client search, shell, custom and discovered function outputs
  edge-input-lifecycle Input validation, timing, repeated steering, old targets
  edge-tools-settings  Pending outputs, recovery, running tools, settings inheritance
  edge-connections     Reconnect, connection-local pending input, lane matching
  vision-steering      Image steering only; requires a vision-capable backend

Options (environment defaults in parentheses):
  --suite smoke|edges|all|non-vision|vision|client-tools|input-lifecycle|tools-settings|connections
                                   (STEERING_SUITE=smoke)
  --case NAME                      One scenario or category (STEERING_CASE)
  --record-set openai|gateway|all   (STEERING_RECORD_SET=openai)
  --gateway-url URL                Gateway base URL (GATEWAY_URL=http://localhost:9000)
  --model NAME                     Override selected provider model
                                   OpenAI: OPENAI_MODEL=gpt-6-sol
                                   Gateway: GATEWAY_MODEL or MODEL (required)
  --unsupported-model NAME         (STEERING_UNSUPPORTED_MODEL=gpt-5.6)
  --output-dir PATH                Exact cassette directory (STEERING_OUTPUT_DIR)
  --repeat N                       Fresh sessions per scenario (STEERING_REPEAT=1)
  --timeout SECONDS                Wall time per scenario, including reconnect
                                   (STEERING_TIMEOUT=300)
  --quiet-seconds SECONDS          Settling window (STEERING_QUIET_SECONDS=3)
  --burst-count N                  Queue-pressure bound (STEERING_BURST_COUNT=32)
  --max-output-tokens N            Per-response cap (MAX_OUTPUT_TOKENS=8192)
  --list-cases                     List the available cases and categories
  --dry-run                        Show selection/output paths; no requests/writes
  --help

smoke records mixed-tools-steer; edges records the three edge categories;
all records all six. non-vision records the five text/tool categories;
client-tools records only client-tools-steer;
vision records only vision-steering. --case edge-cases selects the three non-vision
edge categories. --case array-image-user also selects the vision-steering file.
An individual case still writes its category filename. Reruns replace selected
category files; repeated attempts become labeled sessions inside those files.
Only YAML is generated. Exact prompts and fixtures: multi_turn_steering/prompts.txt.
OPENAI_API_KEY is required only for OpenAI calls. GATEWAY_API_KEY is optional for
an authenticated gateway; keys are never printed or sent to the other provider.
Gateway filenames use gateway instead of openai-reference. unsupported-model is
skipped for gateway because OpenAI model availability is not a gateway contract.
Live calls incur usage.
Acceptance does not prove application. A quiet window is a bounded observation.
Cleanup adds at most five seconds after the per-scenario wall budget.

Examples:
  bash record_multi_turn_steering_cassettes.sh --dry-run --suite all
  GATEWAY_MODEL=your-served-model bash record_multi_turn_steering_cassettes.sh \
    --record-set gateway --suite all --dry-run
  bash record_multi_turn_steering_cassettes.sh --suite edges
  bash record_multi_turn_steering_cassettes.sh --suite tools-settings
  bash record_multi_turn_steering_cassettes.sh --suite client-tools
  GATEWAY_MODEL=Qwen/Qwen3.6-35B-A3B-FP8 bash record_multi_turn_steering_cassettes.sh \
    --record-set gateway --suite non-vision
  GATEWAY_MODEL=Qwen/Qwen3-VL-30B-A3B-Instruct bash record_multi_turn_steering_cassettes.sh \
    --record-set gateway --suite vision
  uv run --no-project --with click --with fastapi --with httpx --with uvicorn \
    --with pyyaml bash record_multi_turn_steering_cassettes.sh --suite all
HELP
}

while (( $# )); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --dry-run) DRY_RUN=true; shift ;;
    --list-cases) LIST_CASES=true; shift ;;
    --record-set|--gateway-url|--suite|--case|--model|--unsupported-model|--output-dir|--repeat|--timeout|--quiet-seconds|--burst-count|--max-output-tokens)
      if (( $# < 2 )); then echo "ERROR: $1 requires a value" >&2; exit 2; fi
      case "$1" in
        --record-set) RECORD_SET="$2" ;; --gateway-url) GATEWAY_BASE="$2" ;;
        --suite) SUITE="$2" ;; --case) CASE="$2" ;; --model) MODEL_OVERRIDE="$2" ;;
        --unsupported-model) UNSUPPORTED_MODEL="$2" ;; --output-dir) OUTPUT_DIR="$2" ;;
        --repeat) REPEAT="$2" ;; --timeout) TIMEOUT="$2" ;;
        --quiet-seconds) QUIET="$2" ;; --burst-count) BURST="$2" ;;
        --max-output-tokens) MAX_TOKENS="$2" ;;
      esac
      shift 2 ;;
    *) echo "ERROR: unknown option $1" >&2; exit 2 ;;
  esac
done

case "$RECORD_SET" in
  openai|gateway|all) ;;
  *) echo 'ERROR: STEERING_RECORD_SET must be openai, gateway, or all' >&2; exit 2 ;;
esac
if [[ "$RECORD_SET" != openai && -z "${MODEL_OVERRIDE:-${GATEWAY_MODEL:-${MODEL:-}}}" && "$LIST_CASES" == false ]]; then
  echo 'ERROR: GATEWAY_MODEL (or MODEL / --model) is required for gateway recordings' >&2
  exit 2
fi

args=(steering --suite "$SUITE" --unsupported-model "$UNSUPPORTED_MODEL"
  --timeout "$TIMEOUT" --quiet-seconds "$QUIET" --burst-count "$BURST" --repeat "$REPEAT"
  --max-output-tokens "$MAX_TOKENS" --output-dir "$OUTPUT_DIR")
if [[ -n "$CASE" ]]; then args+=(--case "$CASE"); fi
if [[ "$DRY_RUN" == true ]]; then args+=(--dry-run); fi
if [[ "$LIST_CASES" == true ]]; then args+=(--list-cases); fi

# Python owns validation, category selection and recording; no duplicated catalog.
status=0
for provider in openai gateway; do
  if [[ "$RECORD_SET" != all && "$RECORD_SET" != "$provider" ]]; then continue; fi
  provider_args=("${args[@]}" --record-set "$provider")
  if [[ -n "$MODEL_OVERRIDE" ]]; then provider_args+=(--model "$MODEL_OVERRIDE"); fi
  if [[ "$provider" == gateway ]]; then provider_args+=(--gateway-url "$GATEWAY_BASE"); fi
  if "$PYTHON" -u "$SCRIPT_DIR/websocket_recorder.py" "${provider_args[@]}"; then
    :
  else
    code=$?
    if (( code == 130 )); then exit "$code"; fi
    status="$code"
  fi
done
exit "$status"
