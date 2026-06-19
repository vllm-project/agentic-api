#!/usr/bin/env bash
set -euo pipefail

OGX_PORT="${OGX_PORT:-8321}"
OGX_PID=""

cleanup() {
    if [ -n "$OGX_PID" ] && kill -0 "$OGX_PID" 2>/dev/null; then
        echo "Stopping OGx (pid $OGX_PID)..."
        kill "$OGX_PID" 2>/dev/null || true
        wait "$OGX_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

OGX_CMD="${OGX_CMD:-ogx}"
OGX_CONFIG="$(cd "$(dirname "$0")" && pwd)/ogx-config.yaml"

echo "Starting OGx on port $OGX_PORT..."
HF_HUB_TRUST_REMOTE_CODE=1 $OGX_CMD run "$OGX_CONFIG" --port "$OGX_PORT" > /tmp/ogx-server.log 2>&1 &
OGX_PID=$!

echo "Waiting for OGx to be ready..."
for i in $(seq 1 60); do
    if curl -sf "http://localhost:$OGX_PORT/v1/health" > /dev/null 2>&1; then
        echo "OGx is ready."
        break
    fi
    if ! kill -0 "$OGX_PID" 2>/dev/null; then
        echo "OGx process exited unexpectedly. Logs:"
        cat /tmp/ogx-server.log
        exit 1
    fi
    sleep 1
done

if ! curl -sf "http://localhost:$OGX_PORT/v1/health" > /dev/null 2>&1; then
    echo "OGx failed to start within 60s. Logs:"
    cat /tmp/ogx-server.log
    exit 1
fi

echo "Running integration tests..."
OGX_BASE_URL="http://localhost:$OGX_PORT" cargo test -p agentic-server --test integration_test -- --nocapture

echo "Integration tests passed."
