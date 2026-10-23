#!/usr/bin/env bash
set -euo pipefail
# Capture real gateway dependencies using the existing recorder, then replay them in Rust.
# Export YOU_API_KEY and start the served model before invoking this script.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/../../../.."
: "${YOU_API_KEY:?Export YOU_API_KEY before recording}"
: "${GATEWAY_MODEL:?Set GATEWAY_MODEL to the served model}"
export PYTHON="${PYTHON:-python3}"
LLM_API_BASE="${LLM_API_BASE:-http://127.0.0.1:5050}"
YOU_API_BASE_URL="${YOU_API_BASE_URL:-https://api.ydc-index.io}"
GATEWAY_BIN="${GATEWAY_BIN:-./target/release/agentic-server}"
OUTPUT_DIR="${MULTI_AGENT_OUTPUT_DIR:-$SCRIPT_DIR/store_false}"
mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"
recorder=crates/agentic-server-core/tests/cassettes/websocket_recorder.py
destination="$(mktemp -d)"
echo "Recording logs and temporary captures: $destination"
mkdir -p "$destination"
"$PYTHON" - "$destination" <<'PY'
import json, sys
from pathlib import Path
p=Path('crates/agentic-server-core/tests/cassettes/multi_agent/mixed_tools.json')
tools=json.loads(p.read_text())
for tool in tools:
    if tool['type']=='mcp': tool['server_url']='http://127.0.0.1:17075/openai/tiktoken'
(Path(sys.argv[1]) / 'tools.json').write_text(json.dumps(tools))
PY
capture_pids=()
cleanup() {
  for pid in "${capture_pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${capture_pids[@]}"; do wait "$pid" 2>/dev/null || true; done
  capture_pids=()
}
trap cleanup EXIT
for mode in nonstreaming streaming; do
  mkdir -p "$destination/$mode"
  "$PYTHON" "$recorder" dependencies --upstream "$LLM_API_BASE" --port 17071 --output "$destination/$mode/model.yaml" > "$destination/$mode/model.log" 2>&1 &
  capture_pids+=("$!")
  "$PYTHON" "$recorder" dependencies --upstream "$YOU_API_BASE_URL" --port 17074 --output "$destination/$mode/search.yaml" > "$destination/$mode/search.log" 2>&1 &
  capture_pids+=("$!")
  "$PYTHON" "$recorder" dependencies --upstream https://gitmcp.io --port 17075 --output "$destination/$mode/mcp.yaml" > "$destination/$mode/mcp.log" 2>&1 &
  capture_pids+=("$!")
  "$PYTHON" - "$destination" <<'PY'
import socket,time
for port in (17071,17074,17075):
    for attempt in range(100):
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=.2): break
        except OSError: time.sleep(.1)
    else: raise RuntimeError(f'proxy {port} did not start')
PY
  YOU_API_BASE_URL=http://127.0.0.1:17074 RUST_LOG=info \
    "$GATEWAY_BIN" --llm-api-base http://127.0.0.1:17071 \
    --gateway-host 127.0.0.1 --gateway-port 19001 \
    --db-url "sqlite://$destination/$mode/gateway.db" > "$destination/$mode/gateway.log" 2>&1 &
  capture_pids+=("$!")
  "$PYTHON" - "$destination" <<'PY'
import socket,time
for attempt in range(100):
    try:
        with socket.create_connection(('127.0.0.1',19001),timeout=.2): break
    except OSError: time.sleep(.1)
else: raise RuntimeError('gateway did not start')
PY
  MULTI_AGENT_RECORD_SET=gateway MULTI_AGENT_SUITE=mixed-tools MULTI_AGENT_STORE=false \
    MULTI_AGENT_STREAM_MODE="$mode" MULTI_AGENT_OUTPUT_DIR="$destination/$mode" \
    MULTI_AGENT_MIXED_TOOLS_FILE="$destination/tools.json" GATEWAY_MODEL="$GATEWAY_MODEL" \
    GATEWAY_URL=http://127.0.0.1:19001 HTTP_READ_TIMEOUT="${HTTP_READ_TIMEOUT:-900}" MAX_OUTPUT_TOKENS="${MAX_OUTPUT_TOKENS:-8192}" PROXY_PORT=17073 \
    bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh > "$destination/$mode/client.log" 2>&1
  cleanup
  for kind in model search mcp; do
    cp "$destination/$mode/$kind.yaml" "$OUTPUT_DIR/multi-agent-store-false-$kind-dependencies-$mode.yaml"
  done
  cp "$destination/$mode/"multi-agent-store-false-gateway-*.yaml "$OUTPUT_DIR/"
  echo "Completed $mode dependency capture; validate the Rust contract and HTTP replay tests."
done
