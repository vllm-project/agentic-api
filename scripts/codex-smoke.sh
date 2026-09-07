#!/usr/bin/env bash
set -euo pipefail

CODEX_BIN="${CODEX_BIN:-codex}"
AGENTIC_BIN="${AGENTIC_BIN:-target/debug/agentic}"
AGENTIC_SERVER_BIN="${AGENTIC_SERVER_BIN:-target/debug/agentic-server}"
PYTHON_BIN="${PYTHON_BIN:-python3}"
CASSETTE="${CASSETTE:-crates/agentic-server-core/tests/cassettes/reasoning/responses/reasoning-single-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml}"
MODEL="${MODEL:-Qwen/Qwen3-30B-A3B-FP8}"

choose_port() {
  "$PYTHON_BIN" -c 'import socket; sock = socket.socket(); sock.bind(("127.0.0.1", 0)); print(sock.getsockname()[1]); sock.close()'
}

REPLAY_PORT="${REPLAY_PORT:-$(choose_port)}"
GATEWAY_PORT="${GATEWAY_PORT:-$(choose_port)}"

if ! command -v "$CODEX_BIN" >/dev/null 2>&1; then
  echo "error: Codex is not installed: ${CODEX_BIN}" >&2
  exit 2
fi
if [[ ! -x "$AGENTIC_SERVER_BIN" ]]; then
  echo "error: agentic-server is not executable: ${AGENTIC_SERVER_BIN}; run cargo build -p agentic-server --bins" >&2
  exit 2
fi
if [[ ! -x "$AGENTIC_BIN" ]]; then
  echo "error: agentic is not executable: ${AGENTIC_BIN}; run cargo build -p agentic-server --bins" >&2
  exit 2
fi
if [[ ! -f "$CASSETTE" ]]; then
  echo "error: Responses cassette not found: ${CASSETTE}" >&2
  exit 2
fi

temp_dir="$(mktemp -d)"
capture_path="${temp_dir}/capture.jsonl"
replay_log="${temp_dir}/replay.log"
gateway_log="${temp_dir}/gateway.log"
codex_output="${temp_dir}/codex.out"
codex_debug="${temp_dir}/codex-debug.log"
replay_pid=""
gateway_pid=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$gateway_pid" ]]; then
    kill "$gateway_pid" >/dev/null 2>&1 || true
    wait "$gateway_pid" >/dev/null 2>&1 || true
  fi
  if [[ -n "$replay_pid" ]]; then
    kill "$replay_pid" >/dev/null 2>&1 || true
    wait "$replay_pid" >/dev/null 2>&1 || true
  fi
  if [[ "$status" -ne 0 ]]; then
    echo "--- replay server log ---" >&2
    sed -n '1,240p' "$replay_log" >&2 || true
    echo "--- agentic-server log ---" >&2
    sed -n '1,240p' "$gateway_log" >&2 || true
    echo "--- Codex output ---" >&2
    sed -n '1,240p' "$codex_output" >&2 || true
    echo "--- Codex debug log ---" >&2
    sed -n '1,240p' "$codex_debug" >&2 || true
    echo "--- replay capture ---" >&2
    sed -n '1,240p' "$capture_path" >&2 || true
  fi
  rm -r "$temp_dir"
  exit "$status"
}
trap cleanup EXIT INT TERM

wait_until_ready() {
  local label="$1"
  local url="$2"
  for attempt in $(seq 1 60); do
    if curl --connect-timeout 1 --max-time 2 --fail --silent "$url" >/dev/null; then
      return 0
    fi
    echo "${label} not ready (attempt ${attempt}/60)"
    sleep 1
  done
  echo "error: ${label} did not become ready" >&2
  return 1
}

"$PYTHON_BIN" scripts/claude_code_replay_server.py serve \
  --cassette "$CASSETTE" \
  --model "$MODEL" \
  --capture "$capture_path" \
  --port "$REPLAY_PORT" \
  >"$replay_log" 2>&1 &
replay_pid=$!
wait_until_ready "replay server" "http://127.0.0.1:${REPLAY_PORT}/health"

env \
  LLM_API_BASE="http://127.0.0.1:${REPLAY_PORT}" \
  GATEWAY_HOST=127.0.0.1 \
  GATEWAY_PORT="$GATEWAY_PORT" \
  SKIP_LLM_READY_CHECK=true \
  DATABASE_URL="sqlite://${temp_dir}/agentic.db" \
  "$AGENTIC_SERVER_BIN" \
  >"$gateway_log" 2>&1 &
gateway_pid=$!
wait_until_ready "agentic-server" "http://127.0.0.1:${GATEWAY_PORT}/ready"

env \
  OPENAI_API_KEY=must-not-be-forwarded \
  AGENTIC_CODEX_BIN="$CODEX_BIN" \
  RUST_LOG=warn \
  "$AGENTIC_BIN" harness codex \
  --gateway-url "http://127.0.0.1:${GATEWAY_PORT}" \
  --model "$MODEL" \
  --quiet \
  -- \
  exec \
  --skip-git-repo-check \
  "Reply with exactly one word: HELLO" \
  >"$codex_output" 2>"$codex_debug" </dev/null

"$PYTHON_BIN" - "$codex_output" <<'PY'
import sys

result = open(sys.argv[1]).read().strip()
assert result == "HELLO", f"expected Codex to print exactly HELLO, got {result!r}"
print(result)
PY

"$PYTHON_BIN" scripts/claude_code_replay_server.py assert-capture \
  --api responses \
  --model "$MODEL" \
  --capture "$capture_path"
