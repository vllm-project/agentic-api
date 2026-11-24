#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

gateway_url="${AGENTIC_GATEWAY_URL:-http://127.0.0.1:3020}"
model="${AGENTIC_MODEL:-${V_MODEL:-}}"
claude_bin="${CLAUDE_BIN:-${AGENTIC_CLAUDE_BIN:-}}"
canonical_model="claude-sonnet-4-5-20250929"
gateway_api_key="${AGENTIC_GATEWAY_API_KEY:-${ANTHROPIC_API_KEY:-}}"
temporary_config_dir=false

if [[ -z "$claude_bin" ]]; then
  claude_bin="$(command -v claude || true)"
fi
if [[ -z "$claude_bin" || ! -x "$claude_bin" ]]; then
  echo 'error: Claude Code not found; set CLAUDE_BIN=/path/to/claude' >&2
  exit 127
fi
for required_command in curl jq; do
  if ! command -v "$required_command" >/dev/null 2>&1; then
    echo "error: $required_command is required to discover the Agentic API model and create Claude Code settings" >&2
    exit 127
  fi
done

umask 077
if [[ -n "${AGENTIC_CLAUDE_CONFIG_DIR:-}" ]]; then
  claude_config_dir="$AGENTIC_CLAUDE_CONFIG_DIR"
  mkdir -p "$claude_config_dir"
else
  claude_config_dir="$(mktemp -d "${TMPDIR:-/tmp}/claude-agentic.XXXXXX")"
  temporary_config_dir=true
fi

models_tmp=""
settings_tmp=""
curl_config_tmp=""
cleanup() {
  local status=$?
  trap - EXIT
  [[ -z "$models_tmp" ]] || rm -f "$models_tmp"
  [[ -z "$settings_tmp" ]] || rm -f "$settings_tmp"
  [[ -z "$curl_config_tmp" ]] || rm -f "$curl_config_tmp"
  if [[ "$temporary_config_dir" == true ]]; then
    find "$claude_config_dir" -depth -delete || true
  fi
  exit "$status"
}
trap cleanup EXIT

models_tmp="$(mktemp "${TMPDIR:-/tmp}/agentic-claude-models.XXXXXX")"
settings_tmp="$(mktemp "${TMPDIR:-/tmp}/agentic-claude-settings.XXXXXX")"

curl_args=(--fail --silent --show-error)
if [[ -n "$gateway_api_key" ]]; then
  if [[ "$gateway_api_key" == *$'\n'* || "$gateway_api_key" == *$'\r'* ]]; then
    echo 'error: AGENTIC_GATEWAY_API_KEY must not contain a line break' >&2
    exit 2
  fi
  curl_config_tmp="$(mktemp "${TMPDIR:-/tmp}/agentic-claude-curl.XXXXXX")"
  escaped_gateway_api_key="${gateway_api_key//\\/\\\\}"
  escaped_gateway_api_key="${escaped_gateway_api_key//\"/\\\"}"
  printf 'header = "Authorization: Bearer %s"\n' "$escaped_gateway_api_key" >"$curl_config_tmp"
  curl_args+=(--config "$curl_config_tmp")
fi
curl "${curl_args[@]}" "${gateway_url%/}/v1/models" >"$models_tmp"
if [[ -z "$model" ]]; then
  model="$(jq --raw-output --exit-status '(.data // .models // [])[0] | (.id // .slug)' "$models_tmp")" || {
    echo 'error: gateway model catalog does not contain a usable first model' >&2
    exit 2
  }
else
  jq --exit-status --arg model "$model" '
    [(.data // .models // [])[] | select((.id // .slug) == $model)]
    | if length == 1 then . else error("gateway model catalog must contain exactly one requested model") end
  ' "$models_tmp" >/dev/null || {
    echo "error: gateway model catalog does not contain requested model: $model" >&2
    exit 2
  }
fi

context_window="$(jq --raw-output --arg model "$model" '
  [(.data // .models // [])[] | select((.id // .slug) == $model)]
  | .[0] | (.max_model_len // .context_length // .context_window // .max_context_window // 32768)
' "$models_tmp")"
if ! [[ "$context_window" =~ ^[1-9][0-9]*$ ]]; then
  context_window=32768
fi

settings_path="$claude_config_dir/agentic-settings.json"
jq --null-input --arg model "$model" --arg canonical_model "$canonical_model" \
  '{modelOverrides: {($canonical_model): $model}}' >"$settings_tmp"
mv "$settings_tmp" "$settings_path"

claude_args=(
  --model "$canonical_model"
  --tools Bash,Edit,Read,WebSearch
  --setting-sources user
  --permission-mode manual
  --effort "${AGENTIC_CLAUDE_EFFORT:-medium}"
  --settings "$settings_path"
)
if [[ "${AGENTIC_YOLO:-0}" == "1" || "${AGENTIC_YOLO:-}" == "true" ]]; then
  claude_args+=(--dangerously-skip-permissions)
fi

unset CLAUDE_CODE_USE_VERTEX ANTHROPIC_VERTEX_PROJECT_ID ANTHROPIC_MODEL
export \
  CLAUDE_CONFIG_DIR="$claude_config_dir" \
  CLAUDE_CODE_EFFORT_LEVEL="${AGENTIC_CLAUDE_EFFORT:-medium}" \
  ANTHROPIC_BASE_URL="$gateway_url" \
  ANTHROPIC_API_KEY="${gateway_api_key:-demo}" \
  ANTHROPIC_AUTH_TOKEN="${ANTHROPIC_AUTH_TOKEN:-${gateway_api_key:-demo}}" \
  ANTHROPIC_MODEL="$model" \
  ANTHROPIC_SMALL_FAST_MODEL="$model" \
  ANTHROPIC_DEFAULT_OPUS_MODEL="$model" \
  ANTHROPIC_DEFAULT_SONNET_MODEL="$model" \
  ANTHROPIC_DEFAULT_HAIKU_MODEL="$model" \
  CLAUDE_CODE_MAX_CONTEXT_TOKENS="$context_window" \
  CLAUDE_CODE_MAX_OUTPUT_TOKENS="${AGENTIC_CLAUDE_MAX_OUTPUT_TOKENS:-2048}" \
  MAX_THINKING_TOKENS=0
"$claude_bin" "${claude_args[@]}" "$@"
