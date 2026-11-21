"""One-off driver: record REAL Anthropic Messages native server-tool pass-through.

Reuses record_cassette.py's embedded recording proxy and cassette writer so all
request/response YAML is captured from a live gateway+upstream exchange, never
hand-authored. The requests carry a native web_search declaration that the
gateway forwards unchanged (MESSAGES_UPSTREAM_SERVER_TOOLS), producing genuine
server_tool_use / web_search_tool_result blocks.

Usage: .venv/bin/python record_409_native_passthrough.py <output.yaml> [--stream]
"""
from __future__ import annotations

import json
import os
import sys

import httpx

# Import the recorder's proxy machinery by module path.
sys.path.insert(0, os.path.dirname(__file__))
import record_cassette as rc  # noqa: E402

GATEWAY = os.environ.get("AGENTIC_GATEWAY_URL", "http://127.0.0.1:9131")
UPSTREAM_KEY = os.environ.get("ANTHROPIC_AUTH_TOKEN", "")
MODEL = os.environ.get("ANTHROPIC_DEFAULT_SONNET_MODEL", "workbuddy-ai/hy4flash")
PROXY_PORT = 7070

NATIVE_SEARCH = {"type": "web_search_20250305", "name": "web_search", "max_uses": 1}


def body(messages: list, tools: list | None, stream: bool) -> dict:
    out: dict = {"model": MODEL, "max_tokens": 2048, "messages": messages, "stream": stream}
    if tools is not None:
        out["tools"] = tools
    return out


def collect_stream(resp: httpx.Response) -> dict:
    """Reconstruct the message from a real SSE stream, mirroring the recorder."""
    message: dict | None = None
    blocks: dict[int, dict] = {}
    partials: dict[int, str] = {}
    for line in resp.iter_lines():
        if not line or not line.startswith("data:"):
            continue
        try:
            event = json.loads(line[5:].strip())
        except Exception:
            continue
        etype = event.get("type")
        if etype == "message_start":
            message = event.get("message", {})
        elif etype == "content_block_start":
            blocks[event["index"]] = event.get("content_block", {})
            partials[event["index"]] = ""
        elif etype == "content_block_delta":
            blk = blocks.setdefault(event["index"], {})
            delta = event.get("delta", {})
            dtype = delta.get("type")
            if dtype in ("text_delta", "thinking_delta", "signature_delta"):
                field = dtype.removesuffix("_delta")
                blk[field] = blk.get(field, "") + delta.get(dtype, "")
            elif dtype == "input_json_delta":
                partials[event["index"]] = partials.get(event["index"], "") + delta.get("partial_json", "")
        elif etype == "message_delta" and message is not None:
            message.update({k: v for k, v in event.get("delta", {}).items()})
    if message is None:
        return {}
    for idx, blk in blocks.items():
        if blk.get("type") in ("tool_use", "server_tool_use"):
            raw = partials.get(idx, "")
            blk["input"] = json.loads(raw) if raw else blk.get("input", {})
    message["content"] = [blocks[i] for i in sorted(blocks)]
    return message


def send(client: httpx.Client, proxy_url: str, payload: dict, stream: bool) -> dict | str:
    if stream:
        with client.stream("POST", f"{proxy_url}/v1/messages", json=payload, timeout=300) as resp:
            resp.raise_for_status()
            return collect_stream(resp)
    resp = client.post(f"{proxy_url}/v1/messages", json=payload, timeout=300)
    resp.raise_for_status()
    return resp.json()


def record_send(client: httpx.Client, output, scratch, payload: dict, stream: bool, attempts: int = 5) -> dict:
    """Record one real exchange; retry model-decided tool use via a scratch
    cassette so non-search attempts never enter the final capture.

    The recording proxy is restarted per attempt against a scratch file. Once
    an attempt actually invokes the native search tool, its single captured
    turn is appended to the final cassette in send order.
    """
    for attempt in range(1, attempts + 1):
        scratch.unlink(missing_ok=True)
        server = rc._start_proxy(scratch, GATEWAY, PROXY_PORT)
        try:
            result = send(client, f"http://{rc.PROXY_HOST}:{PROXY_PORT}", payload, stream)
        finally:
            rc._stop_proxy(server)
        blocks = result.get("content", []) if isinstance(result, dict) else []
        types = [b.get("type") for b in blocks]
        print(f"  attempt {attempt} block types: {types}")
        if "server_tool_use" in types and "web_search_tool_result" in types:
            data = rc.yaml_load(scratch.read_text(encoding="utf-8"))
            turns = data.get("turns", [])
            if len(turns) != 1:
                raise SystemExit(f"expected one captured turn, got {len(turns)}")
            rc._append_turn(output, turns[0])
            return result
    raise SystemExit(f"upstream did not call the native search tool after {attempts} attempts")


def main() -> None:
    from pathlib import Path

    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    stream = "--stream" in sys.argv
    if not args:
        raise SystemExit("usage: record_409_native_passthrough.py <output.yaml> [--stream]")
    output = Path(args[0]).resolve()
    output.unlink(missing_ok=True)
    scratch = output.with_suffix(".scratch.yaml")

    headers = {
        "content-type": "application/json",
        "x-api-key": UPSTREAM_KEY,
        "anthropic-version": "2023-06-01",
    }
    print(f"Recording real pass-through -> {GATEWAY}; stream={stream}")
    with httpx.Client(headers=headers) as client:
        # Turn 1: native declaration passes through, real server-tool blocks returned.
        messages = [{
            "role": "user",
            "content": "Search the web right now for the exact current population of Tokyo. "
                       "You must use the web_search tool. Report only the number.",
        }]
        first = record_send(client, output, scratch, body(messages, [NATIVE_SEARCH], stream), stream)
        blocks = first.get("content", [])

        # Turn 2: replay the blocks the client saw back to the upstream. This
        # exercises the real replay contract for native server-tool blocks:
        # they must pass through unchanged (upstream-owned ids), so the upstream
        # accepts the follow-up and continues the conversation.
        messages2 = list(messages) + [{"role": "assistant", "content": blocks}] + [
            {"role": "user", "content": "Thanks. Answer in one word: what color is the sky?"}
        ]
        server = rc._start_proxy(output, GATEWAY, PROXY_PORT, append=True)
        try:
            second = send(client, f"http://{rc.PROXY_HOST}:{PROXY_PORT}", body(messages2, None, stream), stream)
            types2 = [b.get("type") for b in second.get("content", [])] if isinstance(second, dict) else []
            print("turn2 block types:", types2)
        finally:
            rc._stop_proxy(server)

    scratch.unlink(missing_ok=True)
    captured = rc.yaml_load(output.read_text(encoding="utf-8"))
    print(f"captured {len(captured.get('turns', []))} turns -> {output}")
    for t in captured.get("turns", []):
        r = t["response"]
        print(f"  {t['filename']}: {t['request']['path']} status={r['status_code']} "
              f"sse={bool(r.get('sse'))} keys={list(r.keys())}")


if __name__ == "__main__":
    main()
