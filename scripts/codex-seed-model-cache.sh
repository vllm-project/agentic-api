#!/usr/bin/env bash
set -euo pipefail

GATEWAY_URL="${GATEWAY_URL:-http://127.0.0.1:3018}"
MODEL="${MODEL:-Qwen/Qwen3.6-35B-A3B}"
CODEX_HOME="${CODEX_HOME:-/tmp/agentic-codex-smoke-home}"

if ! command -v realpath >/dev/null 2>&1; then
  echo "error: realpath is required" >&2
  exit 2
fi

temporary_root="$(realpath -m -- /tmp)"
resolved_codex_home="$(realpath -m -- "$CODEX_HOME")"
if [[ "$resolved_codex_home" != "$temporary_root"/* ]]; then
  echo "error: refusing to overwrite a non-temporary Codex model cache: $CODEX_HOME" >&2
  exit 2
fi
CODEX_HOME="$resolved_codex_home"

mkdir -p "$CODEX_HOME"

for command_name in codex curl jq; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "error: $command_name is required" >&2
    exit 2
  fi
done

client_version="$(codex --version | awk '{print $NF}')"
fetched_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
models_url="${GATEWAY_URL%/}/v1/models?client_version=${client_version}"

catalog_tmp="$(mktemp "${CODEX_HOME}/model_catalog.json.tmp.XXXXXX")"
cache_tmp="$(mktemp "${CODEX_HOME}/models_cache.json.tmp.XXXXXX")"
trap 'rm -f "$catalog_tmp" "$cache_tmp"' EXIT

curl --fail --silent --show-error "$models_url" | jq --exit-status \
  --arg model "$MODEL" \
  '{
    models: [.models[] | select(.slug == $model)]
  }
  | if (.models | length) == 1
    then .
    else error("gateway model catalog must contain exactly one requested model")
    end' >"$catalog_tmp"

jq --exit-status \
  --arg client_version "$client_version" \
  --arg fetched_at "$fetched_at" \
  '{
    client_version: $client_version,
    etag: null,
    fetched_at: $fetched_at,
    models
  }' "$catalog_tmp" >"$cache_tmp"

mv --no-target-directory "$catalog_tmp" "${CODEX_HOME}/model_catalog.json"
mv --no-target-directory "$cache_tmp" "${CODEX_HOME}/models_cache.json"
trap - EXIT

echo "Seeded ${CODEX_HOME}/model_catalog.json and models_cache.json with ${MODEL} from ${models_url}"
echo "Use Codex with: -c 'model_catalog_json=\"${CODEX_HOME}/model_catalog.json\"'"
