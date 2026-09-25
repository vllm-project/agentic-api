"""Simulated local-shell output for the mixed-tools recording scenario.

Follows shell/scenarios.py: match the actual requested command to a fixed
fixture, without executing model-generated commands. The recorder submits the
fixture as client input and captures the API's actual response.
"""

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
