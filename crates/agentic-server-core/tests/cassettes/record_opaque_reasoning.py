"""Record the pinned #335 stateless reference matrix through record_cassette.py.

No gateway availability gate is bypassed. Recordings characterize the provider,
then Rust replay tests validate the reserved adapter separately. Run only with a
locally configured OPENAI_API_KEY. This script never displays captured payloads.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import yaml

from recorder_history import CompletedOutput

MODEL = "gpt-5.4-2026-03-05"
REASONING = {"effort": "low", "summary": "concise"}
MAX_FILE_BYTES = 64 * 1024 * 1024
TOOLS = [{
    "type": "function", "name": "lookup_code", "description": "Look up the code for a label.",
    "parameters": {
        "type": "object", "properties": {"label": {"type": "string"}},
        "required": ["label"], "additionalProperties": False,
    }, "strict": True,
}]
SCENARIOS = {
    "continuation": [
        "Remember the number 47. Check that its digits sum to 11 and its reversal is 27 larger. Reply VALID or INVALID.",
        "Add 5 to the number I asked you to remember. Reply with the resulting integer only.",
        "Subtract 5 from the number I asked you to remember. Reply with the resulting integer only.",
    ],
    "function": [
        "Find the two-digit number whose digits sum to 11 and whose reversal is 27 larger. "
        "Then call lookup_code with label ALPHA followed by that number; do not answer without the lookup. "
        "After receiving the code, reply with the code only.",
        "",
        "This is a separate continuation from the call. Reply with the returned code followed by BRANCH.",
    ],
}


def recorded_events(turn: dict, transport: str) -> list[dict]:
    response = turn["response"]
    raw_events = response.get("websocket") if transport == "websocket" else [
        line[5:].strip()
        for entry in response.get("sse", []) for line in entry.splitlines()
        if line.startswith("data:") and line[5:].strip() != "[DONE]"
    ]
    return [json.loads(raw) for raw in raw_events]


def terminal(turn: dict, transport: str) -> dict:
    response = turn["response"]
    if response.get("status_code") != (101 if transport == "websocket" else 200):
        raise ValueError("provider request did not succeed (response details omitted)")
    if transport == "json":
        result = response.get("body")
    else:
        events = recorded_events(turn, transport)
        if any(event.get("type") in {"error", "response.failed", "response.incomplete"} for event in events):
            raise ValueError("provider returned a non-success terminal event")
        completions = [event["response"] for event in events if event.get("type") == "response.completed"]
        if len(completions) != 1:
            raise ValueError("capture must contain exactly one completed response")
        result = completions[0]
    if not isinstance(result, dict) or result.get("status") != "completed" or result.get("model") != MODEL:
        raise ValueError("capture lacks completed, exact pinned-model evidence")
    return result


def validate(path: Path, scenario: str, transport: str) -> int:
    if not path.is_file() or path.stat().st_size > MAX_FILE_BYTES:
        raise ValueError("capture is missing or exceeds the file budget")
    raw = path.read_text(encoding="utf-8")
    key = os.environ.get("OPENAI_API_KEY")
    if key and key in raw:
        raise ValueError("capture contains a credential; it must not be promoted")
    turns = yaml.safe_load(raw)["turns"]
    if len(turns) != 3:
        raise ValueError("expected initial, continuation, and independent branch")
    responses = [terminal(turn, transport) for turn in turns]
    requests = [turn["request"]["body"] for turn in turns]
    for request, response in zip(requests, responses):
        if request.get("model") != MODEL or request.get("reasoning") != REASONING:
            raise ValueError("request does not match the pinned profile")
        if request.get("store") is not False or "previous_response_id" in request or "conversation" in request:
            raise ValueError("capture must use stateless manual item replay")
        if response.get("store") is not False:
            raise ValueError("provider did not report disabled storage")
        reasoning = [item for item in response["output"] if item.get("type") == "reasoning"]
        if not reasoning or any(not item.get("encrypted_content") for item in reasoning):
            raise ValueError("capture lacks opaque reasoning state")
        if transport == "websocket":
            if request.get("type") != "response.create" or "stream" in request:
                raise ValueError("invalid WebSocket request envelope")
        elif request.get("stream") is not (transport == "sse"):
            raise ValueError("invalid HTTP stream setting")
    if transport == "json":
        replay_output = responses[0]["output"]
    else:
        completed = CompletedOutput()
        for event in recorded_events(turns[0], transport):
            completed.observe(event)
        replay_output = completed.replay(responses[0])["output"]
    prefix = requests[0]["input"] + replay_output
    for request in requests[1:]:
        if request["input"][:len(prefix)] != prefix:
            raise ValueError("continuation changed or omitted earlier output items")
    if scenario == "function":
        calls = [item for item in responses[0]["output"] if item.get("type") == "function_call"]
        if len(calls) != 1 or calls[0].get("name") != "lookup_code":
            raise ValueError("expected exactly one lookup_code call")
        for request in requests[1:]:
            output = request["input"][len(prefix)]
            if output != {"type": "function_call_output", "call_id": calls[0]["call_id"], "output": "ORCHID-47"}:
                raise ValueError("function output was not linked to the original call")
        if len(requests[1]["input"]) != len(prefix) + 1 or len(requests[2]["input"]) != len(prefix) + 2:
            raise ValueError("function branch inherited sibling history")
    elif any(len(request["input"]) != len(prefix) + 1 for request in requests[1:]):
        raise ValueError("text branch inherited sibling history")
    return sum(response.get("usage", {}).get("total_tokens", 0) for response in responses)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True, help="Fresh staging directory; existing captures are not replaced")
    parser.add_argument("--proxy-port", type=int, default=17070)
    parser.add_argument("--scenario", choices=tuple(SCENARIOS), action="append")
    parser.add_argument("--transport", choices=("json", "sse", "websocket"), action="append")
    parser.add_argument("--validate-only", action="store_true", help="Validate existing captures without making API requests")
    args = parser.parse_args()
    if not args.validate_only and not os.environ.get("OPENAI_API_KEY"):
        parser.error("OPENAI_API_KEY must be configured locally")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    paths = [(scenario, transport, args.output_dir / f"{scenario}-{transport}.yaml")
             for scenario in (args.scenario or SCENARIOS)
             for transport in (args.transport or ("json", "sse", "websocket"))]
    if not args.validate_only and any(path.exists() for _, _, path in paths):
        parser.error("staging captures already exist; use a fresh directory or --validate-only")
    with tempfile.TemporaryDirectory(prefix="agentic-opaque-inputs-") as temporary:
        inputs = Path(temporary)
        for name, value in (("tools", TOOLS), ("outputs", {"lookup_code": "ORCHID-47"}),
                            ("choices", ["auto", "none", "none"])):
            (inputs / f"{name}.json").write_text(json.dumps(value), encoding="utf-8")
        for scenario, transport, path in paths:
            if not args.validate_only:
                command = [sys.executable, str(Path(__file__).with_name("record_cassette.py")),
                           "--mode", "responses", "--turns", "3", "--no-store", "--manual-item-replay",
                           "--replay-output-source", "item-done",
                           "--openai", "https://api.openai.com", "--model", MODEL,
                           "--reasoning", json.dumps(REASONING), "--max-output-tokens", "1024",
                           "--branch-from", "1", "--branch-turn-number", "3",
                           "--proxy-port", str(args.proxy_port), "--output", str(path),
                           "--stream" if transport != "json" else "--no-stream"]
                if transport == "websocket":
                    command.extend(["--transport", "websocket"])
                if scenario == "function":
                    command.extend(["--tools", str(inputs / "tools.json"), "--tool-outputs", str(inputs / "outputs.json"),
                                    "--tool-choice-sequence", str(inputs / "choices.json"), "--parallel-tool-calls", "false"])
                print(f"Recording {scenario}/{transport} (3 requests, max 1024 output tokens each)", flush=True)
                # Manual replay suppresses payload output. Never echo a child's exception,
                # which could include provider data; the staging capture is kept for inspection.
                result = subprocess.run(command, input="\n".join(SCENARIOS[scenario]) + "\n", text=True,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=1000, check=False)
                if result.returncode:
                    raise ValueError("recorder failed; inspect the staging capture locally, without logging payloads")
            tokens = validate(path, scenario, transport)
            print(f"Validated {scenario}/{transport}: 3 turns, {tokens} total tokens", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error)) from None
