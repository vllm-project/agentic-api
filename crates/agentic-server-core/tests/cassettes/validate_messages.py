"""Check the two-round Messages recording using the recorder's own SSE reconstruction."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys

import httpx
import yaml

RECORDINGS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("messages_recorder", RECORDINGS / "record_cassette.py")
recorder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recorder)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def response_message(turn):
    response = turn["response"]
    require(response["status_code"] == 200, "upstream response must be 200")
    streaming = turn["request"]["body"].get("stream", False)
    require((response.get("sse") is not None) == streaming, "response mode mismatch")
    if not streaming:
        require(response.get("sse") is None, "unexpected SSE")
        return response["body"]
    require(response.get("body") is None, "unexpected JSON body")
    raw = "".join(response["sse"])
    with httpx.Client(transport=httpx.MockTransport(
        lambda request: httpx.Response(200, text=raw, headers={"content-type": "text/event-stream"})
    )) as client, contextlib.redirect_stdout(io.StringIO()):
        return recorder._send_messages_streaming(client, turn["request"]["body"], "http://replay")


def validate(path, model=None):
    turns = yaml.safe_load(Path(path).read_text())["turns"]
    require(len(turns) == 2, "expected exactly two rounds")
    requests = [t["request"]["body"] for t in turns]
    for turn in turns:
        require(turn["request"]["method"] == "POST", "expected POST")
        require(turn["request"]["path"] == "/v1/messages", "expected Messages endpoint")
    first, second = requests
    if model:
        require(first["model"] == model, "recorded model differs from requested model")
    for key in ("model", "tools", "stream", "max_tokens"):
        require(first.get(key) == second.get(key), f"{key} changed between rounds")
    require(len(first["messages"]) == 1 and first["messages"][0]["role"] == "user", "expected one user prompt")
    messages = [response_message(t) for t in turns]
    call_message, final = messages
    require(call_message.get("stop_reason") == "tool_use", "first round must stop for a tool")
    calls = [b for b in call_message["content"] if b["type"] == "tool_use"]
    require(len(calls) == 1, "expected one tool call")
    call = calls[0]
    require(call.get("name") == "web_search", "unexpected tool name")
    require(isinstance(call.get("id"), str) and bool(call["id"].strip()), "missing tool call ID")
    require(isinstance(call.get("input"), dict), "tool input must be an object")
    query = call["input"].get("query")
    require(isinstance(query, str) and bool(query.strip()), "missing search query")
    outputs = json.loads((RECORDINGS / "messages/tool_outputs.json").read_text())
    expected = first["messages"] + [
        {"role": "assistant", "content": call_message["content"]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call["id"],
                                      "content": outputs["web_search"]}]},
    ]
    require(second["messages"] == expected, "second round history or tool output differs")
    require(final.get("stop_reason") == "end_turn", "final round must end_turn")
    require(not any(b["type"] == "tool_use" for b in final["content"]), "unexpected pending tool")
    require(any(b["type"] == "text" and b.get("text", "").strip() for b in final["content"]), "empty final answer")
    return messages


if __name__ == "__main__":
    try:
        validate(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)
    except (ValueError, KeyError, TypeError, IndexError, yaml.YAMLError) as error:
        sys.exit(f"invalid Messages recording: {error}")
    print("validated two-round Messages tool loop")
