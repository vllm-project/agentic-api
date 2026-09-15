#!/usr/bin/env bash
# Record real Dynamo Messages traffic; validate both modes before replacing fixtures.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../../.." && pwd)"
python="${PYTHON:-python3}"
model="${MODEL:-openai/gpt-oss-20b}"
url="${DYNAMO_URL:-http://127.0.0.1:8000}"
out="${OUTPUT_DIR:-$here/dynamo}"
slug="$(printf '%s' "$model" | tr '/: ' '---')"
mkdir -p "$out"
stage="$(mktemp -d "$out/.messages-recording.XXXXXX")"
trap 'echo "Recording staging directory: $stage" >&2' EXIT
for mode in nonstreaming streaming; do
    flag=--no-stream
    [[ "$mode" == streaming ]] && flag=--stream
    file="$stage/dynamo-messages-web-search-$slug-$mode.yaml"
    printf '%s\n' 'What is the latest stable Rust release? Use web_search.' | "$python" "$here/record_cassette.py" \
        --mode messages --turns 2 "$flag" --vllm "$url" --model "$model" \
        --tools "$here/messages/tools.json" --tool-outputs "$here/messages/tool_outputs.json" \
        --max-output-tokens 2048 --proxy-port "${PROXY_PORT:-7070}" --output "$file"
    "$python" "$here/validate_messages.py" "$file" "$model"
done
"$python" "$root/scripts/validate-cassettes.py" "$stage"
for mode in nonstreaming streaming; do
    mv "$stage/dynamo-messages-web-search-$slug-$mode.yaml" "$out/"
done
rmdir "$stage"
trap - EXIT
printf 'Validated Messages recordings installed in %s\n' "$out"
