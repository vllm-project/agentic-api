#!/usr/bin/env bash
# Record short, flat Conversation Items API sequences against OpenAI and a gateway.
# Each filename: tN is one HTTP request in execution order.
# CONVERSATIONS_RECORD_SET=all|openai|gateway (default: all)
# CONVERSATIONS_SCENARIO=all|edge-cases|continuation|continuation-stream|deletion|deletion-stream|branch|branch-stream|pagination|pagination-stream
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CASSETTES_DIR="$SCRIPT_DIR/conversations"
RECORD_SET="${CONVERSATIONS_RECORD_SET:-all}"
SCENARIO="${CONVERSATIONS_SCENARIO:-all}"
OPENAI_MODEL="${OPENAI_MODEL:-gpt-4.1}"
GATEWAY_MODEL="${GATEWAY_MODEL:-$OPENAI_MODEL}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:9000}"
PYTHON_BIN="${PYTHON_BIN:-python3}"

case "$RECORD_SET" in
    all|openai|gateway) ;;
    *) echo "CONVERSATIONS_RECORD_SET must be all, openai, or gateway" >&2; exit 2 ;;
esac
case "$SCENARIO" in
    all|edge-cases|continuation|continuation-stream|deletion|deletion-stream|branch|branch-stream|pagination|pagination-stream) ;;
    *) echo "Invalid CONVERSATIONS_SCENARIO: $SCENARIO" >&2; exit 2 ;;
esac
if [[ "$RECORD_SET" != gateway && -z "${OPENAI_API_KEY:-}" ]]; then
    echo "OPENAI_API_KEY is required for OpenAI recordings" >&2
    exit 2
fi

mkdir -p "$CASSETTES_DIR"
record() {
    local provider="$1" scenario="$2" mode="$3" model="$4"
    local turns=5
    if [[ "$mode" == branch ]]; then turns=6; fi
    if [[ "$mode" == pagination ]]; then turns=10; fi
    if [[ "$mode" == edge-cases ]]; then turns=19; fi
    local destination="$CASSETTES_DIR/conversations-${scenario}-${provider}.yaml"
    local backend_args=()
    if [[ "$provider" == openai ]]; then
        backend_args=(--openai https://api.openai.com)
    else
        backend_args=(--gateway "$GATEWAY_URL")
    fi
    echo "Recording $provider/$scenario -> $destination"
    "$PYTHON_BIN" "$SCRIPT_DIR/record_cassette.py" \
        --mode items --items-scenario "$mode" --turns "$turns" \
        --model "$model" "${backend_args[@]}" \
        --output "$destination" "${@:5}"
    "$PYTHON_BIN" - "$destination" "$mode" "$scenario" <<'CASSETTE_CHECK'
import sys
from pathlib import Path
from yaml import safe_load

cassette = safe_load(Path(sys.argv[1]).read_text())
mode, scenario = sys.argv[2:]
turns = cassette["turns"]
expected = 10 if mode == "pagination" else 6 if mode == "branch" else 19 if mode == "edge-cases" else 5
assert list(cassette) == ["turns"], "cassette must be one flat turns list"
assert len(turns) == expected
assert [turn["filename"] for turn in turns] == [f"t{i}" for i in range(1, expected + 1)]
assert all("status_code" in turn["response"] for turn in turns)
requests = [turn["request"] for turn in turns]
assert (requests[0]["method"], requests[0]["path"]) == ("POST", "/v1/conversations")
if mode == "edge-cases":
    for start, kind, prefix in ((0, "message", "item_wrongprefix_"),
                                (3, "function_call", "msg_wrongprefix_")):
        assert (requests[start]["method"], requests[start]["path"]) == ("POST", "/v1/conversations")
        conv_id = turns[start]["response"]["body"]["id"]
        items_path = f"/v1/conversations/{conv_id}/items"
        assert (requests[start + 1]["method"], requests[start + 1]["path"]) == ("POST", items_path)
        item = requests[start + 1]["body"]["items"][0]
        assert item["type"] == kind and item["id"].startswith(prefix)
        if kind == "function_call":
            assert item["call_id"] == "call_prefix_probe"
        assert (requests[start + 2]["method"], requests[start + 2]["path"]) == ("GET", items_path)
        assert turns[start + 2]["response"]["status_code"] == 200
    assert (requests[6]["method"], requests[6]["path"]) == ("POST", "/v1/conversations")
    conv_id = turns[6]["response"]["body"]["id"]
    items_path = f"/v1/conversations/{conv_id}/items"
    assert (requests[7]["method"], requests[7]["path"]) == ("POST", items_path)
    assert [item["content"] for item in requests[7]["body"]["items"]] == ["msg1", "msg2", "msg3"]
    assert (requests[8]["method"], requests[8]["path"]) == ("GET", items_path)
    assert requests[8]["query_params"] == {"limit": "2"}
    cursor = turns[8]["response"]["body"]["last_id"]
    assert (requests[9]["method"], requests[9]["path"]) == ("DELETE", f"{items_path}/{cursor}")
    assert (requests[10]["method"], requests[10]["path"]) == ("GET", items_path)
    assert requests[10]["query_params"] == {"after": cursor, "limit": "2"}
    reused_id = turns[8]["response"]["body"]["data"][0]["id"]
    assert reused_id != cursor
    for start, count in ((11, 1), (14, 2)):
        assert (requests[start]["method"], requests[start]["path"]) == ("POST", "/v1/conversations")
        target_id = turns[start]["response"]["body"]["id"]
        target_path = f"/v1/conversations/{target_id}/items"
        assert (requests[start + 1]["method"], requests[start + 1]["path"]) == ("POST", target_path)
        items = requests[start + 1]["body"]["items"]
        assert len(items) == count and all(item["id"] == reused_id for item in items)
        assert len({item["content"] for item in items}) == count
        assert (requests[start + 2]["method"], requests[start + 2]["path"]) == ("GET", target_path)
        assert requests[start + 2]["query_params"] == {"order": "asc"}
    assert requests[12]["body"]["items"][0] == requests[15]["body"]["items"][0]
    assert (requests[17]["method"], requests[17]["path"]) == ("POST", items_path)
    assert requests[17]["body"] == requests[12]["body"]
    assert (requests[18]["method"], requests[18]["path"]) == ("GET", items_path)
    assert requests[18]["query_params"] == {"order": "asc"}
    sys.exit(0)
assert (requests[1]["method"], requests[1]["path"]) == ("POST", "/v1/responses")
assert "conversation" in requests[1]["body"] and "previous_response_id" not in requests[1]["body"]
assert requests[1]["body"]["stream"] == scenario.endswith("-stream")
items_path = requests[2]["path"]
assert requests[2]["method"] == "POST" and items_path.endswith("/items")
assert items_path.startswith("/v1/conversations/")
if mode == "pagination":
    assert (requests[3]["method"], requests[3]["path"]) == ("GET", items_path)
    assert requests[3]["query_params"] == {"order": "asc"}
    assert (requests[4]["method"], requests[4]["path"]) == ("GET", items_path)
    assert requests[4]["query_params"] == {"order": "asc", "limit": "2"}
    assert (requests[5]["method"], requests[5]["path"]) == ("GET", items_path)
    assert requests[5]["query_params"] == {"order": "asc", "limit": "2", "after": turns[4]["response"]["body"]["last_id"]}
    assert (requests[6]["method"], requests[6]["path"]) == ("GET", items_path)
    assert requests[6]["query_params"] == {"order": "desc"}
    assert (requests[7]["method"], requests[7]["path"]) == ("GET", items_path)
    assert requests[7]["query_params"] == {"order": "desc", "limit": "2"}
    assert (requests[8]["method"], requests[8]["path"]) == ("GET", items_path)
    assert requests[8]["query_params"] == {"order": "desc", "limit": "2", "after": turns[7]["response"]["body"]["last_id"]}
    assert (requests[9]["method"], requests[9]["path"]) == ("GET", items_path)
    assert requests[9]["query_params"] == {"order": "asc", "include[]": "message.output_text.logprobs"}
elif mode == "deletion":
    assert requests[3]["method"] == "DELETE"
    assert requests[3]["path"].startswith(items_path + "/")
elif mode == "branch":
    assert (requests[3]["method"], requests[3]["path"]) == ("POST", "/v1/responses")
    assert "previous_response_id" in requests[3]["body"]
    assert "conversation" not in requests[3]["body"]
    assert requests[3]["body"]["stream"] == scenario.endswith("-stream")
    assert (requests[4]["method"], requests[4]["path"]) == ("POST", items_path)
else:
    assert (requests[3]["method"], requests[3]["path"]) == ("POST", "/v1/responses")
    assert "conversation" in requests[3]["body"]
    assert "previous_response_id" not in requests[3]["body"]
    assert requests[3]["body"]["stream"] == scenario.endswith("-stream")
if mode != "pagination":
    assert (requests[-1]["method"], requests[-1]["path"]) == ("GET", items_path)
CASSETTE_CHECK
}

for provider in openai gateway; do
    if [[ "$RECORD_SET" != all && "$RECORD_SET" != "$provider" ]]; then continue; fi
    if [[ "$provider" == openai ]]; then model="$OPENAI_MODEL"; else model="$GATEWAY_MODEL"; fi
    for mode in continuation deletion branch pagination; do
        if [[ "$SCENARIO" == all || "$SCENARIO" == "$mode" ]]; then
            record "$provider" "$mode" "$mode" "$model" --no-stream
        fi
        if [[ "$SCENARIO" == all || "$SCENARIO" == "$mode-stream" ]]; then
            record "$provider" "$mode-stream" "$mode" "$model" --stream
        fi
    done
    if [[ "$SCENARIO" == all || "$SCENARIO" == edge-cases ]]; then
        record "$provider" edge-cases edge-cases "$model" --no-stream
    fi
done
