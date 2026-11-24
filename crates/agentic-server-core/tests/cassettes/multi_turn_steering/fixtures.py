"""Steering prompts and workflow fixtures for the WebSocket recorder.

The caller supplies the main recorder's existing loaders and output builder.
This module imports no recorder and performs no transport or cassette writing.
"""
import base64
from functools import lru_cache
from pathlib import Path


STEERING_MIXED_CASES = ("mixed-tools-steer",)

# One source of truth for recording files, independent of transport I/O.
STEERING_CATEGORIES = {
    "mixed-tools-steer": STEERING_MIXED_CASES,
    "client-tools-steer": ("client-tools-steer",),
    "edge-input-lifecycle": (
        "three-turn-steer", "steer-delta", "array-user", "store-false", "late-completed", "burst",
        "unknown-response", "missing-target", "empty-input", "empty-array", "assistant-message",
        "tool-output-input", "extra-field", "stream-id-field", "unsupported-model",
        "old-parent-before-successor", "old-parent-after-successor", "array-multiple-user",
    ),
    "edge-tools-settings": (
        "pending-tool", "pending-withheld", "pending-wrong-output", "pending-output-recovery",
        "settings-automatic", "settings-explicit", "queue-pressure", "hosted-tool-running",
        "pending-user-steer", "pending-steer-then-output", "pending-partial-outputs",
        "settings-explicit-defaults", "pending-output-recovery-store-false",
    ),
    "edge-connections": ("reconnect-target", "disconnect-pending", "lane-parent-mismatch"),
    "vision-steering": ("array-image-user",),
}


@lru_cache(maxsize=1)
def _steering_prompts() -> dict[str, str]:
    sections: dict[str, list[str]] = {}
    current = None
    catalog = Path(__file__).with_name("prompts.txt")
    for line in catalog.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1]
            if current in sections:
                raise ValueError(f"duplicate prompt section: {current}")
            sections[current] = []
        elif current is None:
            raise ValueError("prompt text must follow a [section] in prompts.txt")
        else:
            sections[current].append(line)
    return {name: "\n".join(lines) for name, lines in sections.items()}


def steering_prompt(name: str) -> str:
    value = _steering_prompts().get(name)
    if not value:
        raise ValueError(f"missing or empty [{name}] in multi_turn_steering/prompts.txt")
    return value


def steering_settings_profile(kind: str, *, max_output_tokens: int) -> dict:
    """Observable settings for WebSocket inheritance probes."""
    if kind == "parent":
        return {
            "instructions": steering_prompt("settings-parent-instructions"),
            "tools": [{"type": "function", "name": "get_project_status",
                       "description": steering_prompt("function-description"),
                       "parameters": {"type": "object", "properties": {}, "required": [],
                                      "additionalProperties": False}, "strict": True}],
            "tool_choice": "auto", "parallel_tool_calls": False,
            "reasoning": {"effort": "low"}, "store": True,
            "max_output_tokens": min(max_output_tokens, 2048), "max_tool_calls": 2,
            "text": {"format": {"type": "json_schema", "name": "steering_settings",
                                "strict": True, "schema": {
                                    "type": "object", "properties": {
                                        "markers": {"type": "array", "items": {"type": "string"}},
                                        "answer": {"type": "string"}},
                                    "required": ["markers", "answer"], "additionalProperties": False}}},
        }
    if kind == "explicit":
        return {
            "instructions": steering_prompt("settings-explicit-instructions"),
            "tools": [], "tool_choice": "auto", "parallel_tool_calls": True,
            "reasoning": {"effort": "medium"}, "store": False,
            "max_output_tokens": min(max_output_tokens, 3072), "max_tool_calls": 4,
            "text": {"format": {"type": "text"}},
        }
    raise ValueError(f"unknown steering settings profile: {kind!r}")


def _steering_fixture_directory(case: str) -> Path:
    if case not in STEERING_MIXED_CASES:
        raise ValueError(f"unknown mixed-tool steering scenario: {case!r}")
    return Path(__file__).resolve().parents[1] / "parallel_tool_calls"


def steering_request_settings(case: str, *, load_tools) -> dict:
    """Original mixed-tool workflow settings; model and transport belong to the caller."""
    if case == "client-tools-steer":
        directory = Path(__file__).resolve().parents[1] / "multi_agent"
        tools = load_tools(directory / "client_owned_tools.json")
        tools.extend(tool for tool in load_tools(directory / "mixed_tools.json") if tool["type"] == "shell")
        return {"input": steering_prompt(case), "store": True, "tools": tools,
                "parallel_tool_calls": False, "tool_choice": "auto"}
    directory = _steering_fixture_directory(case)
    return {
        "input": steering_prompt(case),
        "store": True,
        "tools": load_tools(directory / "tools-mixed.json"),
        "parallel_tool_calls": True,
        "tool_choice": "auto",
    }


def steering_tool_outputs(case: str, calls: list[dict], *, load_tools, load_outputs, build_outputs) -> list[dict]:
    """Use the existing recorder handlers with captured calls; never execute generated commands."""
    if case == "client-tools-steer":
        directory = Path(__file__).resolve().parents[1]
        return build_outputs(calls, load_outputs(directory / "multi_agent" / "mixed_tool_outputs.py"),
                             user_prompt=None, tool_search_tools=load_tools(directory / "tool_search" / "returned_tools.json"))
    directory = _steering_fixture_directory(case)
    return build_outputs(calls, load_outputs(directory / "tool_outputs.py"), user_prompt=None)



def steering_media_input(case: str) -> list[dict]:
    """Real local media embedded in a user message; no uploads or HTTP setup."""
    if case == "array-image-user":
        payload = Path(__file__).resolve().parents[1] / "images" / "inputs" / "red-blue-64.png"
        part = {"type": "input_image", "image_url": "data:image/png;base64," +
                base64.b64encode(payload.read_bytes()).decode("ascii"), "detail": "low"}
        prompt = steering_prompt("ws-image-steer")
    else:
        raise ValueError(f"unknown steering media case: {case!r}")
    return [{"role": "user", "content": [{"type": "input_text", "text": prompt}, part]}]
