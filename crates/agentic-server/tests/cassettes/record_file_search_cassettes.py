"""Record vLLM Responses API cassettes for the file_search integration test."""

from __future__ import annotations

import datetime as dt
import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


DEFAULT_MODEL = "openai/gpt-oss-20b"
DEFAULT_PROMPT = "Use the file_search tool to find information about Rust memory safety ownership."
DEFAULT_OUTPUT = Path(__file__).with_name("file_search") / "vllm-file-search-openai-gpt-oss-20b.json"

SEARCH_RESULT = {
    "results": [
        {
            "file_id": "file_abc",
            "filename": "rust-memory-safety.txt",
            "score": 0.95,
            "attributes": {},
            "content": [
                {
                    "type": "text",
                    "text": "Rust enforces memory safety without a garbage collector through ownership, borrowing, and lifetimes.",
                }
            ],
        }
    ]
}


def post_json(base_url: str, payload: dict[str, Any]) -> dict[str, Any]:
    url = f"{base_url.rstrip('/')}/v1/responses"
    data = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(url, data=data, headers={"content-type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=300) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        body = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"vLLM request failed with HTTP {err.code}: {body}") from err


def file_search_tool() -> dict[str, Any]:
    return {
        "type": "function",
        "name": "file_search",
        "description": "Search uploaded files for relevant passages.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query.",
                }
            },
            "required": ["query"],
        },
    }


def find_file_search_call(response: dict[str, Any]) -> dict[str, Any]:
    for item in response.get("output", []):
        if item.get("type") == "function_call" and item.get("name") == "file_search":
            return item
    raise RuntimeError(f"recorded response did not include a file_search function call: {response}")


def main() -> int:
    vllm_url = os.environ.get("VLLM_URL", "http://localhost:8000")
    model = os.environ.get("MODEL", DEFAULT_MODEL)
    output = Path(os.environ.get("OUTPUT", DEFAULT_OUTPUT)).resolve()
    prompt = os.environ.get("PROMPT", DEFAULT_PROMPT)
    tools = [file_search_tool()]

    first_request = {
        "model": model,
        "input": prompt,
        "tools": tools,
        "tool_choice": "auto",
        "stream": False,
    }
    first_response = post_json(vllm_url, first_request)
    call = find_file_search_call(first_response)

    second_request = {
        "model": model,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": prompt,
            },
            call,
            {
                "type": "function_call_output",
                "call_id": call["call_id"],
                "output": json.dumps(SEARCH_RESULT, separators=(",", ":")),
            },
        ],
        "tools": tools,
        "tool_choice": "auto",
        "stream": False,
    }
    second_response = post_json(vllm_url, second_request)

    cassette = {
        "metadata": {
            "recorded_at": dt.datetime.now(dt.UTC).isoformat(timespec="seconds"),
            "model": model,
            "note": "Harmony models reject tool_choice=required, so this cassette uses tool_choice=auto with a direct prompt.",
        },
        "turns": [
            {
                "request": {
                    "method": "POST",
                    "path": "/v1/responses",
                    "body": first_request,
                },
                "response": {
                    "status_code": 200,
                    "body": first_response,
                },
            },
            {
                "request": {
                    "method": "POST",
                    "path": "/v1/responses",
                    "body": second_request,
                },
                "response": {
                    "status_code": 200,
                    "body": second_response,
                },
            },
        ],
    }

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(cassette, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"recorded file_search cassette -> {output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
