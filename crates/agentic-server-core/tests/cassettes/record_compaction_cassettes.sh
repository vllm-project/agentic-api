#!/usr/bin/env bash
# Record compaction scenarios against OpenAI and a gateway, one flat cassette per scenario and provider.
# Each filename: tN is one HTTP request in execution order.
# COMPACTION_RECORD_SET=all|openai|gateway (default: all)
# COMPACTION_SCENARIO=all|stale|in-turn-chain|in-turn-chain-stream|tool-loop|standalone|multi-agent|store-false|mcp
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CASSETTES_DIR="$SCRIPT_DIR/compaction"
RECORD_SET="${COMPACTION_RECORD_SET:-all}"
SCENARIO="${COMPACTION_SCENARIO:-all}"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4.1}"
# The multi-agent beta needs a model that supports it.
MULTI_AGENT_OPENAI_MODEL="${MULTI_AGENT_OPENAI_MODEL:-gpt-5.6-sol}"
# Async client tools require a supporting OpenAI model.
TOOL_LOOP_OPENAI_MODEL="${TOOL_LOOP_OPENAI_MODEL:-gpt-6-astra}"
GATEWAY_MODEL="${GATEWAY_MODEL:-$OPENAI_MODEL}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
PYTHON_BIN="${PYTHON_BIN:-python3}"

case "$RECORD_SET" in
    all|openai|gateway) ;;
    *) echo "COMPACTION_RECORD_SET must be all, openai, or gateway" >&2; exit 2 ;;
esac
case "$SCENARIO" in
    all|stale|in-turn-chain|in-turn-chain-stream|tool-loop|standalone|multi-agent|store-false|mcp) ;;
    *) echo "Invalid COMPACTION_SCENARIO: $SCENARIO" >&2; exit 2 ;;
esac
if [[ "$RECORD_SET" != gateway && -z "${OPENAI_API_KEY:-}" ]]; then
    echo "OPENAI_API_KEY is required for OpenAI recordings" >&2
    exit 2
fi

mkdir -p "$CASSETTES_DIR"
# record PROVIDER SCENARIO ITEMS_SCENARIO TURNS MODEL [recorder flags...]
record() {
    local provider="$1" scenario="$2" items_scenario="$3" turns="$4" model="$5"
    local destination="$CASSETTES_DIR/compaction-${scenario}-${provider}.yaml"
    local backend_args=()
    if [[ "$provider" == openai ]]; then
        backend_args=(--openai https://api.openai.com)
    else
        backend_args=(--gateway "$GATEWAY_URL")
    fi
    echo "Recording $provider/$scenario -> $destination"
    "$PYTHON_BIN" "$SCRIPT_DIR/record_cassette.py" \
        --mode items --items-scenario "$items_scenario" --turns "$turns" \
        --model "$model" "${backend_args[@]}" --proxy-port "${RECORDER_PROXY_PORT:-7070}" \
        --output "$destination" "${@:6}"
    "$PYTHON_BIN" - "$destination" "$turns" <<'CASSETTE_CHECK'
import sys
from pathlib import Path
from yaml import safe_load

cassette = safe_load(Path(sys.argv[1]).read_text())
assert list(cassette) == ["turns"], "cassette must be one flat turns list"
assert len(cassette["turns"]) == int(sys.argv[2]), len(cassette["turns"])
CASSETTE_CHECK
}

selected() { [[ "$SCENARIO" == all || "$SCENARIO" == "$1" ]]; }

for provider in openai gateway; do
    if [[ "$RECORD_SET" != all && "$RECORD_SET" != "$provider" ]]; then continue; fi
    model="$GATEWAY_MODEL"
    multi_agent_model="$GATEWAY_MODEL"
    tool_loop_model="$GATEWAY_MODEL"
    if [[ "$provider" == openai ]]; then
        model="$OPENAI_MODEL"
        multi_agent_model="$MULTI_AGENT_OPENAI_MODEL"
        tool_loop_model="$TOOL_LOOP_OPENAI_MODEL"
    fi
    if selected stale; then record "$provider" stale stale-compaction 10 "$model" --stream; fi
    if selected in-turn-chain; then record "$provider" in-turn-chain compaction-in-turn-chain 5 "$model" --no-stream; fi
    if selected in-turn-chain-stream; then
        record "$provider" in-turn-chain-stream compaction-in-turn-chain 5 "$model" --stream
    fi
    if selected tool-loop; then record "$provider" tool-loop compaction-tool-loop 10 "$tool_loop_model" --no-stream; fi
    if selected standalone; then record "$provider" standalone compaction-standalone 5 "$model" --no-stream; fi
    if selected multi-agent; then
        record "$provider" multi-agent compaction-multi-agent 5 "$multi_agent_model" --no-stream
    fi
    if selected store-false; then record "$provider" store-false compaction-store-false 2 "$model" --stream; fi
    if selected mcp; then record "$provider" mcp compaction-mcp 11 "$model" --no-stream; fi
done
