"""Simulated client function, custom-tool, and shell outputs for mixed-tools recording.

Follows shell/scenarios.py: match the actual requested command to a fixed
fixture, without executing model-generated commands. The recorder submits the
fixture as client input and captures the API's actual response.
"""

import json
from pathlib import Path


COMMAND = "python3 -c 'print(sum(range(1, 11)))'"


def shell(action: dict) -> dict:
    """Return success or an explicit fixture failure per command, in request order."""
    commands = action.get("commands")
    if not isinstance(commands, list) or not commands:
        raise ValueError("Expected a nonempty shell commands array")
    result = {
        "output": [
            {"stdout": "55\n", "stderr": "", "outcome": {"type": "exit", "exit_code": 0}}
            if command == COMMAND
            else {
                "stdout": "",
                "stderr": (
                    f"Simulated shell fixture: unsupported command {command!r}; nothing was executed. "
                    f"Each commands entry must be a complete shell command. Supported command: {COMMAND}\n"
                ),
                "outcome": {"type": "exit", "exit_code": 1},
            }
            for command in commands
        ]
    }
    if action.get("max_output_length") is not None:
        result["max_output_length"] = action["max_output_length"]
    return result


def _paris_result(name: str, city: str) -> str:
    # Accept the country-qualified spelling without matching other cities named Paris.
    normalized_city = ",".join(part.strip().casefold() for part in city.split(","))
    if normalized_city not in {"paris", "paris,france"}:
        raise ValueError(f"No simulated {name} output for city: {city!r}")
    fixtures = Path(__file__).resolve().parent.parent / "tool_search/function_outputs.json"
    return json.loads(fixtures.read_text(encoding="utf-8"))[name]


def get_weather(city: str) -> str:
    """Return the existing Paris weather fixture, not a live observation."""
    return _paris_result("get_weather", city)


def get_timezone(city: str) -> str:
    """Return the existing Paris time-zone fixture."""
    return _paris_result("get_timezone", city)


agentic_ns__travel__get_timezone = get_timezone


def agentic_raw_echo() -> str:
    """Return the existing single-line custom-tool fixture."""
    fixtures = Path(__file__).resolve().parent.parent / "custom_tool/tool_outputs.json"
    return json.loads(fixtures.read_text(encoding="utf-8"))["agentic_raw_echo"]
