#!/usr/bin/env python3
"""
Record a real OpenAI Chat Completions 2-step tool loop for MCP-wrapper replay fixtures.

This script is intended to be used with `scripts/recorder_proxy.py` so the resulting
YAML files can be curated into `responses/tests/cassettes/chat_completion/`.

Flow:
1) Step 1 (streaming): force a tool call to `mcp__...` wrapper function.
2) Step 2 (streaming): send tool output and let the model produce final assistant text.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from openai import OpenAI


def _load_dotenv_if_available(path: Path | None) -> None:
    if path is None:
        return
    try:
        from dotenv import load_dotenv  # type: ignore
    except Exception:
        return
    should_override = os.getenv("OPENAI_API_KEY") in (None, "", "DUMMY")
    load_dotenv(dotenv_path=path, override=should_override)


def _utc_run_id(prefix: str) -> str:
    ts = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    return f"{ts}-{prefix}"


def _stream_chat_and_collect(
    client: OpenAI,
    *,
    model: str,
    messages: list[dict[str, Any]],
    tools: list[dict[str, Any]],
    tool_choice: str | dict[str, Any],
) -> tuple[str, list[dict[str, Any]]]:
    stream = client.chat.completions.create(
        model=model,
        messages=messages,
        tools=tools,
        tool_choice=tool_choice,
        stream=True,
        stream_options={"include_usage": True},
    )

    assistant_content = ""
    tool_calls: list[dict[str, Any]] = []
    for chunk in stream:
        if not chunk.choices:
            continue
        choice = chunk.choices[0]
        delta = choice.delta

        if delta.content:
            assistant_content += delta.content

        if not delta.tool_calls:
            continue

        for tool_call_delta in delta.tool_calls:
            idx = tool_call_delta.index
            while len(tool_calls) <= idx:
                tool_calls.append(
                    {
                        "id": "",
                        "type": "function",
                        "function": {"name": "", "arguments": ""},
                    }
                )

            if tool_call_delta.id:
                tool_calls[idx]["id"] = tool_call_delta.id

            if tool_call_delta.function:
                if tool_call_delta.function.name:
                    tool_calls[idx]["function"]["name"] = tool_call_delta.function.name
                if tool_call_delta.function.arguments:
                    tool_calls[idx]["function"]["arguments"] += tool_call_delta.function.arguments

    return assistant_content, tool_calls


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Record OpenAI Chat Completions MCP-wrapper cassettes via recorder proxy."
    )
    dotenv_group = parser.add_mutually_exclusive_group()
    dotenv_group.add_argument(
        "--dotenv",
        type=Path,
        default=Path(".env"),
        help="Load env from dotenv file (default: .env).",
    )
    dotenv_group.add_argument(
        "--no-dotenv",
        action="store_const",
        const=None,
        dest="dotenv",
        help="Disable dotenv loading.",
    )
    parser.add_argument(
        "--base-url",
        default=os.getenv("OPENAI_BASE_URL", "http://127.0.0.1:8234/v1"),
        help="OpenAI base URL, usually your recorder proxy /v1 endpoint.",
    )
    parser.add_argument(
        "--model",
        default=os.getenv("OPENAI_MODEL", "gpt-5-nano"),
        help="Model for recording.",
    )
    parser.add_argument(
        "--tool-name",
        default="mcp__github_docs__search_docs",
        help="Function tool name to record (MCP wrapper-style).",
    )
    parser.add_argument(
        "--query",
        default="migration notes",
        help="Search query argument sent through the wrapper tool call.",
    )
    parser.add_argument(
        "--run-prefix",
        default="mcp-chat",
        help="Prefix used to create x-run-id headers.",
    )
    parser.add_argument(
        "--recorder-output-dir",
        default="/tmp/openai-mcp",
        help="Recorder output dir (used only for post-run hints).",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    _load_dotenv_if_available(args.dotenv)

    api_key = os.getenv("OPENAI_API_KEY")
    if not api_key:
        raise SystemExit("OPENAI_API_KEY is missing.")

    tools = [
        {
            "type": "function",
            "function": {
                "name": args.tool_name,
                "description": "Hosted MCP wrapper tool",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                    "additionalProperties": False,
                },
            },
        }
    ]

    step1_run_id = _utc_run_id(f"{args.run_prefix}-step1")
    client1 = OpenAI(
        api_key=api_key,
        base_url=args.base_url,
        default_headers={"x-run-id": step1_run_id},
        timeout=600,
    )
    step1_messages: list[dict[str, Any]] = [
        {
            "role": "user",
            "content": (
                f"Call tool `{args.tool_name}` with query={args.query!r}. "
                "Do not answer directly before tool call."
            ),
        }
    ]
    assistant_content, tool_calls = _stream_chat_and_collect(
        client1,
        model=args.model,
        messages=step1_messages,
        tools=tools,
        tool_choice={"type": "function", "function": {"name": args.tool_name}},
    )
    if not tool_calls:
        raise RuntimeError("Step 1 did not produce any tool call.")

    tc = tool_calls[0]
    tc_name = tc.get("function", {}).get("name")
    if tc_name != args.tool_name:
        raise RuntimeError(f"Unexpected tool name in step1: {tc_name!r}")
    tc_id = tc.get("id")
    if not tc_id:
        raise RuntimeError("Step 1 tool call id is missing.")

    args_raw = tc.get("function", {}).get("arguments") or "{}"
    try:
        parsed_args = json.loads(args_raw)
    except json.JSONDecodeError:
        parsed_args = {"query": args.query}

    step2_messages = [
        *step1_messages,
        {"role": "assistant", "content": assistant_content or "", "tool_calls": tool_calls},
        {
            "role": "tool",
            "tool_call_id": tc_id,
            "name": args.tool_name,
            "content": json.dumps(
                {
                    "results": [
                        {
                            "title": "Migration Notes",
                            "url": "https://example.test/migration",
                            "query": parsed_args.get("query", args.query),
                        }
                    ]
                },
                ensure_ascii=False,
                separators=(",", ":"),
            ),
        },
    ]

    step2_run_id = _utc_run_id(f"{args.run_prefix}-step2")
    client2 = OpenAI(
        api_key=api_key,
        base_url=args.base_url,
        default_headers={"x-run-id": step2_run_id},
        timeout=600,
    )
    _ = _stream_chat_and_collect(
        client2,
        model=args.model,
        messages=step2_messages,
        tools=tools,
        tool_choice="auto",
    )

    out_dir = Path(args.recorder_output_dir)
    sys.stdout.write("Recorded MCP-wrapper chat completion flow.\n")
    sys.stdout.write(f"step1 run-id: {step1_run_id}\n")
    sys.stdout.write(f"step2 run-id: {step2_run_id}\n")
    sys.stdout.write(f'Find files with:\n  rg -n "{step1_run_id}|{step2_run_id}" {out_dir}\n')
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
