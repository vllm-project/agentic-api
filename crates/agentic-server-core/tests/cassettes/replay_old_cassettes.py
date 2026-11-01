"""Replay existing tool scenarios through the shared recorder."""

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


def run_multi_agent_store_false(client, proxy_url, model, stream, *, run_responses,
                                load_tool_outputs, max_output_tokens=None):
    """Replay an original multi-agent request, carrying live output history each turn."""
    root = Path(__file__).parent
    try:
        max_continuations = int(os.environ.get("MULTI_AGENT_MAX_CONTINUATIONS", "10"))
    except ValueError as error:
        raise ValueError("MULTI_AGENT_MAX_CONTINUATIONS must be an integer from 1 to 100") from error
    if not 1 <= max_continuations <= 100:
        raise ValueError("MULTI_AGENT_MAX_CONTINUATIONS must be an integer from 1 to 100")
    scenario = os.environ.get("MULTI_AGENT_SUITE", "mixed-tools")
    if scenario == "client-agent-message":
        token = "CLIENT_AGENT_MESSAGE_9f21"
        request = {"headers": {"OpenAI-Beta": "responses_multi_agent=v1"}, "body": {
            "multi_agent": {"enabled": True, "max_concurrent_subagents": 3},
            "input": [
                {"role": "user", "content": (
                    "Client agent message probe: spawn one subagent named token_checker. "
                    "Ask it to verify the secret test token from the client-supplied agent_message; "
                    "include that message in its assignment if necessary. Wait for the subagent's "
                    "answer, then reply with only the token. If the message is unavailable, "
                    "reply with only NO_AGENT_MESSAGE."
                )},
                {"type": "agent_message", "author": "/root", "recipient": "/root",
                 "agent": {"agent_name": "/root"},
                 "content": [{"type": "input_text", "text": f"The secret test token is {token}."}]},
            ],
        }}
    else:
        if scenario not in {"mixed-tools", "tool-search", "code-interpreter"}:
            raise ValueError(f"Unknown multi-agent replay suite: {scenario}")
        source_scenario = "client-owned-tools" if scenario == "tool-search" else scenario
        source = root / f"multi_agent/multi-agent-openai-reference-{source_scenario}-gpt-5.6-sol-nonstreaming.yaml"
        request = yaml_load(source.read_text(encoding="utf-8"))["turns"][0]["request"]
    body = copy.deepcopy(request["body"])
    if scenario == "tool-search":
        body["input"] += (
            " As /root, first discover get_weather and travel.get_timezone yourself with client tool_search. "
            "Then spawn exactly three children with fork_turns none and bounded assignments: "
            "paris_weather must discover get_weather if needed, invoke it for Paris, and report its actual result; "
            "paris_timezone must discover travel.get_timezone if needed, invoke it for Paris, and report its actual result; "
            "cassette_echo must invoke agentic_raw_echo with CUSTOM_CASSETTE_OK and report its actual output. "
            "Do not copy these root orchestration instructions into child messages. "
            "Only /root may end a response with PENDING_CLIENT_RESULTS. After spawning, /root may wait once "
            "for at most 10000 ms, then should return PENDING_CLIENT_RESULTS if children are unfinished. "
            "This lets the client send their tool results on the next request. "
            "Never interrupt the children. Children must not finalize with PENDING_CLIENT_RESULTS; "
            "they must invoke their own tools and read the actual results before returning a final answer."
        )
    client.headers.update({key: value for key, value in request["headers"].items()
                           if key.lower() == "openai-beta"})
    options = {
        "multi_agent": copy.deepcopy(body["multi_agent"]),
        "reasoning": copy.deepcopy(body.get("reasoning")),
        "max_output_tokens": max_output_tokens,
        "tool_outputs": load_tool_outputs(root / "multi_agent/mixed_tool_outputs.py"),
        "manual_item_replay": True,
        "auto_tool_continuations": max_continuations,
    }
    client_tools = copy.deepcopy(body.get("tools"))
    returned_tools = (json.loads((root / "tool_search/returned_tools.json").read_text())
                      if scenario == "tool-search" else None)
    history = run_responses(
        client, 1, model, stream, False, [], proxy_url,
        tools=client_tools, tool_choice=body.get("tool_choice"),
        parallel_tool_calls=body.get("parallel_tool_calls"),
        preset_input=copy.deepcopy(body["input"]),
        tool_search_output_tools=returned_tools, **options,
    )
    if scenario != "tool-search":
        return

    called = {(item.get("type"), item.get("name")) for item in history}
    required = {("function_call", "get_weather"), ("function_call", "get_timezone"),
                ("custom_tool_call", "agentic_raw_echo")}
    if required - called:
        followup = (
            "Finish the original three client-tool tasks before moving on. Reuse the existing children; "
            "use followup_task to resume them if needed. The weather child must invoke get_weather for Paris, "
            "the timezone child must invoke travel.get_timezone for Paris, and the echo child must return "
            "the actual agentic_raw_echo output. Tool discovery results are now in the supplied history. "
            "Do not treat discovery alone as task completion. Only /root may return PENDING_CLIENT_RESULTS "
            "to allow client callbacks; children must read their actual tool results before finalizing."
        )
        history = run_responses(
            client, 1, model, stream, False, [], proxy_url, tools=client_tools,
            preset_input=history + [{"role":"user", "content":followup}],
            tool_search_output_tools=returned_tools, **options,
        )
        called = {(item.get("type"), item.get("name")) for item in history}
    if missing := required - called:
        raise ValueError(f"Tool-search client phase did not execute required tools: {sorted(missing)}")

    # Keep the client definitions and full history, switching only search execution and adding the existing MCP fixture.
    hosted = json.loads((root / "hosted_tool_search/inputs/tools-mcp-deferred.json").read_text())
    tools = [copy.deepcopy(tool) if tool["type"] != "tool_search" else {"type": "tool_search"}
             for tool in client_tools]
    tools.extend(tool for tool in hosted if tool["type"] == "mcp")
    prompt = (
        "Continue with hosted tool search, which runs on the service. Spawn one new child named docs_worker "
        "with fork_turns none. Give it this bounded assignment: do the work yourself without spawning children; "
        "use tool_search to load mcp_gitmcp_tiktoken, call search_tiktoken_documentation to look up encoding "
        "and decoding, then make a fresh get_weather function call for Paris and read its actual client result. "
        "If get_weather is not visible, load its path with hosted tool_search. Do not reuse weather from an "
        "earlier assignment or claim that a tool was called without actually calling it. "
        "Keep HTTP response instructions out of the child's assignment. Only /root may return "
        "PENDING_CLIENT_RESULTS after at most one 10000 ms wait, allowing the client's next request. "
        "Never interrupt a child waiting for a client result."
    )
    hosted_start = len(history)
    history = run_responses(
        client, 1, model, stream, False, [], proxy_url, tools=tools,
        preset_input=history + [{"role":"user", "content":prompt}], **options,
    )
    if not any(item.get("type") == "function_call" and item.get("name") == "get_weather"
               and item.get("agent", {}).get("agent_name", "").startswith("/root/docs_worker")
               for item in history[hosted_start:]):
        # A genuine follow-up keeps a model's premature summary visible rather than fabricating a tool call.
        followup = (
            "Use followup_task on the existing docs_worker to request exactly one fresh get_weather call "
            "for city Paris, loading the get_weather path with hosted tool_search if needed. "
            "The docs_worker must issue the call itself and read the client result; a quotation of old weather "
            "is not an execution. Do not spawn another agent. Only /root may return PENDING_CLIENT_RESULTS "
            "after at most one 10000 ms wait so the client can submit the tool result."
        )
        history = run_responses(
            client, 1, model, stream, False, [], proxy_url, tools=tools,
            preset_input=history + [{"role":"user", "content":followup}], **options,
        )
    continuation = (
        "The client results are now in the full history. Use followup_task on the existing docs_worker "
        "to call search_tiktoken_documentation through MCP once more, checking encode versus decode with "
        "its already loaded tools. This must be a fresh MCP execution after the client result, not a "
        "recap of the earlier lookup. Do not spawn another agent or request weather again. "
        "Wait for its findings, then summarize the documentation and weather results."
    )
    history = run_responses(
        client, 1, model, stream, False, [], proxy_url, tools=tools,
        preset_input=history + [{"role":"user", "content":continuation}], **options,
    )
    hosted_items = history[hosted_start:]
    mcp_calls = [index for index, item in enumerate(hosted_items) if item.get("type") == "mcp_call"]
    weather_ids = {item["call_id"] for item in hosted_items
                   if item.get("type") == "function_call" and item.get("name") == "get_weather"
                   and (item.get("agent", {}).get("agent_name") == "/root/docs_worker"
                        or item.get("agent", {}).get("agent_name", "").startswith("/root/docs_worker/"))}
    if len(mcp_calls) < 2 or not any(
        item.get("type") == "function_call_output" and item.get("call_id") in weather_ids
        and mcp_calls[0] < index < mcp_calls[-1]
        for index, item in enumerate(hosted_items)
    ):
        raise ValueError("Hosted tool-search phase must execute MCP before and after its branch receives a client result")
    if sum(item.get("type") == "mcp_list_tools" for item in hosted_items) != 1:
        raise ValueError("Hosted tool-search phase must emit exactly one MCP discovery listing")
