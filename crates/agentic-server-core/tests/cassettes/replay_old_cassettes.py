"""Replay existing tool scenarios through the shared recorder, then list inputs."""

import copy
import json
import os
from pathlib import Path

from yaml import safe_load as yaml_load


def run_tool_items(client, proxy_url, model, *, send_request, extract_tool_calls,
                   load_tool_outputs, build_tool_output_input):
    root = Path(__file__).parent
    references = (
        "parallel_tool_calls/parallel-builtin-only-openai-reference-gpt-5.6-nonstreaming.yaml",
        "parallel_tool_calls/parallel-client-openai-reference-gpt-5.6-nonstreaming.yaml",
        "parallel_tool_calls/parallel-mixed-openai-reference-gpt-5.6-nonstreaming.yaml",
        "shell/shell-openai-reference-success-gpt-5.6-nonstreaming.yaml",
        "code_interpreter/code-interpreter-openai-reference-gpt-5.6-nonstreaming.yaml",
        "tool_search/tool-search-openai-reference-gpt-5.6-nonstreaming.yaml",
        "multi_agent/multi-agent-openai-reference-review-gpt-5.6-sol-nonstreaming.yaml",
    )
    for filename in references:
        turns = yaml_load((root / filename).read_text(encoding="utf-8"))["turns"]
        if filename.startswith("tool_search/"):
            fixtures = load_tool_outputs(root / "tool_search/function_outputs.json")
            search_tools = json.loads((root / "tool_search/returned_tools.json").read_text())
        else:
            fixtures = load_tool_outputs(root / ("shell/scenarios.py" if filename.startswith("shell/")
                                                 else "parallel_tool_calls/tool_outputs.py"))
            search_tools = None
        headers = {key: value for key, value in turns[0]["request"]["headers"].items()
                   if key.lower() == "openai-beta"}
        scenario_model = os.environ.get("RESPONSES_MULTI_AGENT_MODEL", model) if filename.startswith("multi_agent/") else model
        # The last response's output appears in the following response's input
        # page. Resolve any pending client calls using the existing fake tools.
        followup = copy.deepcopy(turns[-1]["request"]["body"])
        followup.update(previous_response_id=turns[-1]["response"]["body"]["id"],
                        input="Do not call tools again. Reply only OK.", tool_choice="none")
        responses = {}
        for turn in [*turns, {"request": {"body": followup}}]:
            body = copy.deepcopy(turn["request"]["body"])
            body.update(model=scenario_model, store=True, stream=False)
            previous = body.get("previous_response_id")
            if previous:
                parent = responses[previous]
                body["previous_response_id"] = parent["id"]
                # Preserve recorded user messages, but answer the live calls;
                # models can choose different batches and ordering of calls.
                user_input = body["input"]
                if isinstance(user_input, str):
                    user_input = [{"type": "message", "role": "user", "content": user_input}]
                else:
                    user_input = [item for item in user_input if item.get("type") == "message"]
                body["input"] = build_tool_output_input(
                    extract_tool_calls(parent), fixtures, None, search_tools
                ) + user_input
            print(f"[tool execution: {filename}; continuation: {bool(previous)}]", flush=True)
            result = send_request(client, proxy_url, "POST", "/v1/responses",
                                  body=body, headers=headers, expected_status=None)
            if not result.get("id"):
                print(f"[tool execution failed: {result.get('error')}]", flush=True)
                break
            params = {"order": "asc", "limit": 100}
            while True:
                page = send_request(client, proxy_url, "GET", f"/v1/responses/{result['id']}/input_items",
                                    params=params, expected_status=None)
                if not page.get("has_more"):
                    break
                cursor = page.get("last_id")
                if not cursor or cursor == params.get("after"):
                    raise RuntimeError("Input-items pagination made no progress")
                params["after"] = cursor
            recorded = turn.get("response", {}).get("body")
            if recorded is not None:
                responses[recorded["id"]] = result
