#!/usr/bin/env bash
set -euo pipefail

require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    echo "error: set ${name}" >&2
    exit 2
  fi
}

require_env V_API_BASE

if [[ "$V_API_BASE" != *"://"* ]]; then
  V_API_BASE="http://${V_API_BASE}"
fi

GATEWAY_HOST="${GATEWAY_HOST:-127.0.0.1}"
GATEWAY_PORT="${GATEWAY_PORT:-3000}"
DATABASE_URL="${DATABASE_URL:-sqlite://./agentic_api_codex.db}"
SKIP_LLM_READY_CHECK="${SKIP_LLM_READY_CHECK:-true}"

if [[ -n "${V_API_KEY:-}" ]]; then
  export OPENAI_API_KEY="${OPENAI_API_KEY:-$V_API_KEY}"
else
  unset OPENAI_API_KEY
fi
export DATABASE_URL

echo "Starting agentic-api gateway on http://${GATEWAY_HOST}:${GATEWAY_PORT}"
echo "Upstream base: ${V_API_BASE}"
if [[ -n "${V_MODEL:-}" ]]; then
  echo "Gateway-facing model: ${V_MODEL}"
fi
echo "Skip readiness check: ${SKIP_LLM_READY_CHECK}"

ready_args=()
if [[ "$SKIP_LLM_READY_CHECK" == "true" || "$SKIP_LLM_READY_CHECK" == "1" ]]; then
  ready_args+=(--skip-llm-ready-check)
fi

exec cargo run -p agentic-server -- \
  --gateway-host "$GATEWAY_HOST" \
  --gateway-port "$GATEWAY_PORT" \
  --llm-api-base "$V_API_BASE" \
  "${ready_args[@]}"
