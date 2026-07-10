#!/usr/bin/env python3
"""Small stdio MCP server used to verify Codex namespace tool round-trips."""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path
from typing import Any


SERVER_NAME = "agentic_fixture"
SERVER_VERSION = "0.1.0"
REPO_ROOT = Path(os.environ.get("AGENTIC_FIXTURE_ROOT", Path(__file__).resolve().parents[1])).resolve()
SKIP_DIRS = {".git", "target", "__pycache__", "codex_captures"}
MAX_READ_BYTES = 12_000

TOOLS = [
    {
        "name": "run",
        "description": "Echo a command string for agentic-api Codex namespace round-trip validation.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "cmd": {"type": "string"},
            },
            "required": ["cmd"],
            "additionalProperties": False,
        },
    },
    {
        "name": "echo_text",
        "description": "Echo text with basic metadata. Useful for proving a simple MCP function call worked.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "uppercase": {"type": "boolean", "default": False},
            },
            "required": ["text"],
            "additionalProperties": False,
        },
    },
    {
        "name": "add_numbers",
        "description": "Add a list of numbers and return the total.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "numbers": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 1,
                },
            },
            "required": ["numbers"],
            "additionalProperties": False,
        },
    },
    {
        "name": "make_slug",
        "description": "Turn text into a lowercase URL/file-name friendly slug.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "separator": {"type": "string", "default": "-"},
            },
            "required": ["text"],
            "additionalProperties": False,
        },
    },
    {
        "name": "repo_file_head",
        "description": "Read the first lines of a repository file, limited to the agentic-api workspace.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "lines": {"type": "integer", "minimum": 1, "maximum": 80, "default": 20},
            },
            "required": ["path"],
            "additionalProperties": False,
        },
    },
    {
        "name": "search_repo",
        "description": "Literal text search across repository files, returning a small capped result set.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "path_prefix": {"type": "string", "default": "."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 30, "default": 10},
            },
            "required": ["query"],
            "additionalProperties": False,
        },
    },
]


def read_message() -> tuple[dict[str, Any], str] | None:
    headers: dict[str, str] = {}
    first_line = sys.stdin.buffer.readline()
    if first_line == b"":
        return None
    stripped = first_line.strip()
    if stripped.startswith(b"{"):
        return json.loads(stripped.decode("utf-8")), "line"

    line = first_line.decode("ascii", "replace").strip()
    if line:
        name, _, value = line.partition(":")
        headers[name.lower()] = value.strip()

    while True:
        line = sys.stdin.buffer.readline()
        if line == b"":
            return None
        line = line.decode("ascii", "replace").strip()
        if not line:
            break
        name, _, value = line.partition(":")
        headers[name.lower()] = value.strip()

    length = int(headers.get("content-length", "0"))
    if length <= 0:
        return None
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode("utf-8")), "content-length"


def write_message(message: dict[str, Any], framing: str) -> None:
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    if framing == "line":
        sys.stdout.buffer.write(body + b"\n")
        sys.stdout.buffer.flush()
        return
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def result_response(request_id: Any, result: dict[str, Any]) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def error_response(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def text_result(text: str) -> dict[str, Any]:
    return {"content": [{"type": "text", "text": text}], "isError": False}


def json_text_result(value: Any) -> dict[str, Any]:
    return text_result(json.dumps(value, indent=2, sort_keys=True))


def argument_error(request_id: Any, message: str) -> dict[str, Any]:
    return error_response(request_id, -32602, message)


def string_arg(arguments: dict[str, Any], name: str) -> str | None:
    value = arguments.get(name)
    return value if isinstance(value, str) else None


def int_arg(arguments: dict[str, Any], name: str, default: int, minimum: int, maximum: int) -> int:
    value = arguments.get(name, default)
    if not isinstance(value, int):
        return default
    return max(minimum, min(maximum, value))


def resolve_repo_path(relative_path: str) -> Path | None:
    candidate = (REPO_ROOT / relative_path).resolve()
    try:
        candidate.relative_to(REPO_ROOT)
    except ValueError:
        return None
    return candidate


def repo_relative(path: Path) -> str:
    return str(path.relative_to(REPO_ROOT))


def read_limited_text(path: Path) -> str:
    with path.open("rb") as fh:
        return fh.read(MAX_READ_BYTES).decode("utf-8", "replace")


def handle_run(arguments: dict[str, Any], request_id: Any) -> dict[str, Any]:
    cmd = string_arg(arguments, "cmd")
    if cmd is None:
        return argument_error(request_id, "missing string argument: cmd")
    return result_response(request_id, text_result(f"agentic_fixture.run received cmd={cmd}"))


def handle_echo_text(arguments: dict[str, Any], request_id: Any) -> dict[str, Any]:
    text = string_arg(arguments, "text")
    if text is None:
        return argument_error(request_id, "missing string argument: text")
    echoed = text.upper() if arguments.get("uppercase") is True else text
    return result_response(
        request_id,
        json_text_result({"echo": echoed, "characters": len(text), "words": len(text.split())}),
    )


def handle_add_numbers(arguments: dict[str, Any], request_id: Any) -> dict[str, Any]:
    numbers = arguments.get("numbers")
    if not isinstance(numbers, list) or not numbers:
        return argument_error(request_id, "missing non-empty array argument: numbers")
    if not all(isinstance(number, (int, float)) and not isinstance(number, bool) for number in numbers):
        return argument_error(request_id, "numbers must contain only numeric values")
    total = sum(numbers)
    return result_response(request_id, json_text_result({"count": len(numbers), "sum": total}))


def handle_make_slug(arguments: dict[str, Any], request_id: Any) -> dict[str, Any]:
    text = string_arg(arguments, "text")
    if text is None:
        return argument_error(request_id, "missing string argument: text")
    separator = string_arg(arguments, "separator") or "-"
    separator = separator[:1] or "-"
    slug = re.sub(r"[^a-z0-9]+", separator, text.lower()).strip(separator)
    return result_response(request_id, json_text_result({"slug": slug}))


def handle_repo_file_head(arguments: dict[str, Any], request_id: Any) -> dict[str, Any]:
    relative = string_arg(arguments, "path")
    if relative is None:
        return argument_error(request_id, "missing string argument: path")
    path = resolve_repo_path(relative)
    if path is None or not path.is_file():
        return argument_error(request_id, f"file not found inside repo: {relative}")

    line_limit = int_arg(arguments, "lines", 20, 1, 80)
    text = read_limited_text(path)
    selected = text.splitlines()[:line_limit]
    numbered = "\n".join(f"{idx + 1}: {line}" for idx, line in enumerate(selected))
    return result_response(
        request_id,
        text_result(f"{repo_relative(path)} first {len(selected)} lines:\n{numbered}"),
    )


def iter_search_files(base: Path) -> Any:
    for path in base.rglob("*"):
        if any(part in SKIP_DIRS for part in path.relative_to(REPO_ROOT).parts):
            continue
        if path.is_file():
            yield path


def handle_search_repo(arguments: dict[str, Any], request_id: Any) -> dict[str, Any]:
    query = string_arg(arguments, "query")
    if not query:
        return argument_error(request_id, "missing non-empty string argument: query")
    prefix = string_arg(arguments, "path_prefix") or "."
    base = resolve_repo_path(prefix)
    if base is None or not base.exists():
        return argument_error(request_id, f"path_prefix not found inside repo: {prefix}")
    max_results = int_arg(arguments, "max_results", 10, 1, 30)

    matches = []
    files = [base] if base.is_file() else iter_search_files(base)
    for path in files:
        try:
            text = read_limited_text(path)
        except OSError:
            continue
        for line_no, line in enumerate(text.splitlines(), start=1):
            if query in line:
                matches.append({"path": repo_relative(path), "line": line_no, "text": line.strip()[:240]})
                if len(matches) >= max_results:
                    return result_response(request_id, json_text_result({"query": query, "matches": matches}))

    return result_response(request_id, json_text_result({"query": query, "matches": matches}))


TOOL_HANDLERS = {
    "run": handle_run,
    "echo_text": handle_echo_text,
    "add_numbers": handle_add_numbers,
    "make_slug": handle_make_slug,
    "repo_file_head": handle_repo_file_head,
    "search_repo": handle_search_repo,
}


def handle_request(message: dict[str, Any]) -> dict[str, Any] | None:
    request_id = message.get("id")
    method = message.get("method")

    if request_id is None:
        return None

    if method == "initialize":
        return result_response(
            request_id,
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        )

    if method == "ping":
        return result_response(request_id, {})

    if method == "tools/list":
        return result_response(request_id, {"tools": TOOLS})

    if method == "tools/call":
        params = message.get("params") if isinstance(message.get("params"), dict) else {}
        name = params.get("name")
        arguments = params.get("arguments") if isinstance(params.get("arguments"), dict) else {}
        handler = TOOL_HANDLERS.get(name)
        if handler is None:
            return error_response(request_id, -32602, f"unknown tool: {name}")
        return handler(arguments, request_id)

    if method in {"resources/list", "prompts/list"}:
        key = "resources" if method == "resources/list" else "prompts"
        return result_response(request_id, {key: []})

    return error_response(request_id, -32601, f"method not found: {method}")


def main() -> int:
    while True:
        received = read_message()
        if received is None:
            return 0
        message, framing = received
        response = handle_request(message)
        if response is not None:
            write_message(response, framing)


if __name__ == "__main__":
    raise SystemExit(main())
