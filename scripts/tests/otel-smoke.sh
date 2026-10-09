#!/usr/bin/env bash
# End-to-end check of gateway telemetry against the local Collector.
#
#   docker compose -f deploy/otel/docker-compose.yaml up -d
#   scripts/tests/otel-smoke.sh
#
# Starts a scripted upstream and the gateway exporting OTLP to the Collector,
# sends streamed, blocking, proxied, and WebSocket requests, stops the gateway
# so it flushes, then checks the Collector's debug output and Prometheus
# endpoint: every gateway instrument arrived with this run's service.name and
# only allow-listed attribute keys and values.
#
# AGENTIC_SERVER_BIN selects a prebuilt gateway; otherwise it is built with
# cargo. Requires docker, curl, and python3.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
if docker compose version >/dev/null 2>&1; then
    compose=(docker compose -f "$root/deploy/otel/docker-compose.yaml")
else
    compose=(docker-compose -f "$root/deploy/otel/docker-compose.yaml")
fi
fixtures="$root/scripts/tests/otel_fixtures.py"
# Unique per run, so output left by earlier runs cannot satisfy the check.
service="agentic-otel-smoke-$$"
work=$(mktemp -d)
pids=()

cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    rm -rf "$work"
}
trap cleanup EXIT

wait_for() {
    local what=$1 url=$2
    for _ in $(seq 1 150); do
        if curl -fs -o /dev/null "$url"; then
            return 0
        fi
        sleep 0.2
    done
    echo "timed out waiting for $what at $url" >&2
    return 1
}

wait_for "the Collector (docker compose -f deploy/otel/docker-compose.yaml up -d)" http://127.0.0.1:13133/

bin=${AGENTIC_SERVER_BIN:-}
if [[ -z "$bin" ]]; then
    cargo build --quiet --manifest-path "$root/Cargo.toml" -p agentic-server --bin agentic-server
    bin="$root/target/debug/agentic-server"
fi

upstream_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
gateway_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')

python3 "$fixtures" upstream "$upstream_port" &
pids+=($!)
wait_for "the scripted upstream" "http://127.0.0.1:$upstream_port/health"

env \
    AGENTIC_API_HOME="$work" \
    RUST_LOG=warn \
    OTEL_TRACES_EXPORTER=otlp \
    OTEL_METRICS_EXPORTER=otlp \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
    OTEL_SERVICE_NAME="$service" \
    "$bin" \
    --llm-api-base "http://127.0.0.1:$upstream_port" \
    --skip-llm-ready-check \
    --gateway-host 127.0.0.1 \
    --gateway-port "$gateway_port" \
    >"$work/gateway.log" 2>&1 &
gateway_pid=$!
pids+=("$gateway_pid")
wait_for "the gateway" "http://127.0.0.1:$gateway_port/health"

python3 "$fixtures" requests "$gateway_port"

# SIGTERM drains the gateway and flushes both signals before it exits.
kill -TERM "$gateway_pid"
if ! wait "$gateway_pid"; then
    echo "gateway exited unsuccessfully:" >&2
    cat "$work/gateway.log" >&2
    exit 1
fi

# The Collector batches before exporting; poll until this run's metrics show up.
for _ in $(seq 1 50); do
    "${compose[@]}" logs --no-color otel-collector >"$work/collector.log" 2>&1
    curl -fsS http://127.0.0.1:8889/metrics >"$work/prometheus.txt"
    if grep -q "service.name: Str($service)" "$work/collector.log" &&
        grep -q "service_name=\"$service\"" "$work/prometheus.txt"; then
        break
    fi
    sleep 0.2
done

python3 "$fixtures" check "$service" "$work/collector.log" "$work/prometheus.txt"
