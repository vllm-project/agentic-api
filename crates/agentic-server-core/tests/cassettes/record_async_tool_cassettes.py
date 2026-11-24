#!/usr/bin/env python3
"""Record async client tool scenarios through the shared recording proxy.

Guide: https://developers.openai.com/api/docs/guides/async-tool-calling

Async function and custom tools let the model continue after emitting a call; the client returns the
result later on the original call_id. These scenarios need control the prompt-driven recorder does not
offer: a result held back across follow-ups, probes whose status is recorded rather than required, and
outputs built from earlier call IDs. Each request still goes through record_cassette.py's proxy, which
captures it unchanged with authorization masked.

Every request carries an `x-run-id` header of the form `<scenario>/<step>`. The proxy records it, so
tests can identify each step without relying on positions alone.

Record OpenAI first, review the observations, then implement and compare gateway behavior.
`--provider vllm --sample N` records nothing: it sends a model server the store:false, full-history requests the
gateway sends while an async call is pending, with and without the hints, and prints how the model continued.

Usage from the repository root (OPENAI_API_KEY exported):
  uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
    python crates/agentic-server-core/tests/cassettes/record_async_tool_cassettes.py
  ... --scenario function-delayed-result --stream-mode nonstreaming
  ... --provider gateway --base-url http://localhost:9000 --model <served-model>
  ... --provider vllm --base-url http://localhost:8000 --model Qwen/Qwen3.6-35B-A3B --sample 16
"""

from __future__ import annotations

import argparse
import copy
import json
import os
import re
import socket
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

import httpx
import yaml

SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import record_cassette  # noqa: E402
from record_cassette import _start_proxy, _stop_proxy  # noqa: E402

FIXTURES_DIR = SCRIPT_DIR / "async_tools"
DEFAULT_OPENAI_MODEL = "gpt-6-astra"
DEFAULT_UNSUPPORTED_MODEL = "gpt-5.5"
MULTI_AGENT_BETA = "responses_multi_agent=v1"
READ_TIMEOUT = 900
MAX_OUTPUT_TOKENS = 4096
# Open reasoning models (vLLM directly or behind the gateway) can deliberate past 4096 tokens before answering;
# the multi-agent recorder uses 16384 too.
VLLM_MAX_OUTPUT_TOKENS = 16384


def load_fixture(name: str) -> Any:
    return json.loads((FIXTURES_DIR / name).read_text(encoding="utf-8"))


TOOLS = load_fixture("tools.json")
PROMPTS = load_fixture("prompts.json")


@dataclass
class Exchange:
    """One recorded request's outcome as the scenario sees it."""

    status: int
    response: dict | None
    error: Any = None

    @property
    def ok(self) -> bool:
        """HTTP 200 with a completed response (not incomplete or failed)."""
        return self.status == 200 and self.response is not None and self.response.get("status") == "completed"

    @property
    def output(self) -> list[dict]:
        return (self.response or {}).get("output") or []

    def calls(self, item_type: str, name: str | None = None) -> list[dict]:
        return [
            item
            for item in self.output
            if item.get("type") == item_type and (name is None or item.get("name") == name)
        ]

    def text(self) -> str:
        parts = []
        for item in self.output:
            if item.get("type") != "message":
                continue
            for part in item.get("content") or []:
                if part.get("type") == "output_text":
                    parts.append(part.get("text", ""))
        return "\n".join(parts)


@dataclass
class Session:
    """Sends one scenario's requests through the recording proxy."""

    client: httpx.Client
    proxy_url: str
    model: str
    stream: bool
    scenario: str
    headers: dict[str, str]
    unsupported_model: str = DEFAULT_UNSUPPORTED_MODEL
    max_output_tokens: int = MAX_OUTPUT_TOKENS
    problems: list[str] = field(default_factory=list)

    def send(
        self,
        step: str,
        *,
        input_value: Any,
        tools: list | None,
        instructions: str | None = None,
        previous: Exchange | None = None,
        model: str | None = None,
        extra: dict | None = None,
        beta: str | None = None,
        store: bool = True,
    ) -> Exchange:
        body: dict[str, Any] = {
            "model": model or self.model,
            "input": input_value,
            "store": store,
            "stream": self.stream,
            "max_output_tokens": self.max_output_tokens,
        }
        if tools is not None:
            body["tools"] = copy.deepcopy(tools)
        if instructions is not None:
            body["instructions"] = instructions
        if previous is not None:
            if not previous.ok:
                raise ScenarioAbort(f"{step}: previous step has no response to continue from")
            body["previous_response_id"] = previous.response["id"]
        if extra:
            body.update(copy.deepcopy(extra))
        headers = dict(self.headers)
        headers["x-run-id"] = f"{self.scenario}/{step}"
        if beta:
            headers["OpenAI-Beta"] = beta
        print(f"  -> {self.scenario}/{step}", flush=True)
        exchange = self._post(body, headers)
        summary = ", ".join(
            f"{item.get('type')}{'(async)' if item.get('async') else ''}" for item in exchange.output
        )
        print(f"     status={exchange.status} output=[{summary}]", flush=True)
        return exchange

    def _post(self, body: dict, headers: dict[str, str]) -> Exchange:
        url = f"{self.proxy_url}/v1/responses"
        timeout = httpx.Timeout(READ_TIMEOUT, read=READ_TIMEOUT + 10)
        if not self.stream:
            response = self.client.post(url, json=body, headers=headers, timeout=timeout)
            payload = _json_or_text(response)
            if response.status_code == 200 and isinstance(payload, dict):
                return Exchange(200, payload)
            _reject_proxy_failure(payload)
            return Exchange(response.status_code, None, payload)
        with self.client.stream("POST", url, json=body, headers=headers, timeout=timeout) as response:
            if response.status_code != 200:
                # Drain fully so the proxy finishes writing this turn before we continue.
                response.read()
                payload = _json_or_text(response)
                _reject_proxy_failure(payload)
                return Exchange(response.status_code, None, payload)
            terminal: dict | None = None
            error: Any = None
            for line in response.iter_lines():
                if not line.startswith("data:") or line == "data: [DONE]":
                    continue
                try:
                    event = json.loads(line[5:].strip())
                except json.JSONDecodeError:
                    continue
                kind = event.get("type")
                if kind in {"response.completed", "response.incomplete", "response.failed"}:
                    terminal = event.get("response")
                elif kind == "error":
                    error = event
            return Exchange(200, terminal, error)

    def expect(self, condition: bool, message: str) -> None:
        if not condition:
            self.problems.append(message)
            print(f"     ! {message}", flush=True)

    def require(self, condition: bool, message: str) -> None:
        if not condition:
            raise ScenarioAbort(message)


class ScenarioAbort(Exception):
    """A step did not produce what later steps depend on; the recording is unusable."""


def _json_or_text(response: httpx.Response) -> Any:
    try:
        return response.json()
    except ValueError:
        return response.text


def _reject_proxy_failure(payload: Any) -> None:
    """A proxy transport failure (e.g. read timeout) is not an upstream observation."""
    error = payload.get("error") if isinstance(payload, dict) else None
    if isinstance(error, dict) and error.get("type") == "cassette_proxy_transport_error":
        raise ScenarioAbort(f"recorder proxy failed: {error.get('message')}")


def function_output(call_id: str, output: str) -> dict:
    return {"type": "function_call_output", "call_id": call_id, "output": output}


def custom_output(call_id: str, output: str) -> dict:
    return {"type": "custom_tool_call_output", "call_id": call_id, "output": output}


def user(text: str) -> dict:
    return {"type": "message", "role": "user", "content": text}


def start_async_weather(session: Session, step: str = "t1-start") -> tuple[Exchange, dict]:
    """Ask for the async weather call plus independent work; return the response and the call."""
    first = session.send(
        step,
        input_value=PROMPTS["weather_start"],
        tools=TOOLS["weather_async"],
        instructions=PROMPTS["instructions"]["weather"],
    )
    session.require(first.ok, f"{step}: expected HTTP 200, got {first.status}: {first.error}")
    calls = first.calls("function_call", "get_weather")
    session.require(len(calls) == 1, f"{step}: expected one get_weather call, got {len(calls)}")
    session.require(calls[0].get("async") is True, f"{step}: get_weather call is not marked async")
    return first, calls[0]


# ── scenarios ────────────────────────────────────────────────────────────────


def scenario_function_delayed_result(session: Session) -> None:
    """Async function call, two follow-ups without its result, then the late result."""
    first, call = start_async_weather(session)
    session.expect(bool(first.text()), "t1: no independent answer alongside the async call")
    second = session.send(
        "t2-follow-up-without-output",
        input_value=PROMPTS["weather_follow_up_1"],
        tools=TOOLS["weather_async"],
        instructions=PROMPTS["instructions"]["weather"],
        previous=first,
    )
    session.require(second.ok, f"t2: follow-up without the pending output returned {second.status}")
    third = session.send(
        "t3-follow-up-without-output",
        input_value=PROMPTS["weather_follow_up_2"],
        tools=TOOLS["weather_async"],
        instructions=PROMPTS["instructions"]["weather"],
        previous=second,
    )
    session.require(third.ok, f"t3: second follow-up without the pending output returned {third.status}")
    late = session.send(
        "t4-late-output",
        input_value=[function_output(call["call_id"], PROMPTS["weather_output"]), user(PROMPTS["weather_ask"])],
        tools=TOOLS["weather_async"],
        instructions=PROMPTS["instructions"]["weather"],
        previous=third,
    )
    session.require(late.ok, f"t4: late output on the original call_id returned {late.status}")
    session.expect("22" in late.text(), "t4: answer does not use the late weather result (22 C)")


def scenario_custom_delayed_result(session: Session) -> None:
    """Async custom tool call, a follow-up without its result, then the late result."""
    first = session.send(
        "t1-start",
        input_value=PROMPTS["custom_start"],
        tools=TOOLS["custom_async"],
        instructions=PROMPTS["instructions"]["custom"],
    )
    session.require(first.ok, f"t1: expected HTTP 200, got {first.status}: {first.error}")
    calls = first.calls("custom_tool_call", "agentic_async_echo")
    session.require(len(calls) == 1, f"t1: expected one agentic_async_echo call, got {len(calls)}")
    session.require(calls[0].get("async") is True, "t1: custom tool call is not marked async")
    session.expect(bool(first.text()), "t1: no independent answer alongside the async call")
    second = session.send(
        "t2-follow-up-without-output",
        input_value=PROMPTS["custom_follow_up"],
        tools=TOOLS["custom_async"],
        instructions=PROMPTS["instructions"]["custom"],
        previous=first,
    )
    session.require(second.ok, f"t2: follow-up without the pending output returned {second.status}")
    late = session.send(
        "t3-late-output",
        input_value=[
            custom_output(calls[0]["call_id"], PROMPTS["custom_output"]),
            user("What did the echo job return? Reply with only its result."),
        ],
        tools=TOOLS["custom_async"],
        instructions=PROMPTS["instructions"]["custom"],
        previous=second,
    )
    session.require(late.ok, f"t3: late custom output returned {late.status}")
    session.expect(PROMPTS["custom_output"] in late.text(), "t3: answer does not use the late echo result")


def scenario_web_search_mixed(session: Session) -> None:
    """An async client call and a hosted web search in one response, then the late result."""
    first = session.send(
        "t1-start",
        input_value=PROMPTS["web_search_start"],
        tools=TOOLS["weather_async_with_web_search"],
        instructions=PROMPTS["instructions"]["weather"],
    )
    session.require(first.ok, f"t1: expected HTTP 200, got {first.status}: {first.error}")
    calls = first.calls("function_call", "get_weather")
    session.require(len(calls) == 1, f"t1: expected one get_weather call, got {len(calls)}")
    session.expect(bool(first.calls("web_search_call")), "t1: no web_search_call in the same response")
    late = session.send(
        "t2-late-output",
        input_value=[function_output(calls[0]["call_id"], PROMPTS["weather_output"]), user(PROMPTS["weather_ask"])],
        tools=TOOLS["weather_async_with_web_search"],
        instructions=PROMPTS["instructions"]["weather"],
        previous=first,
    )
    session.require(late.ok, f"t2: late output returned {late.status}")


def scenario_wait_tool(session: Session) -> None:
    """Two async lookups and an application-defined wait tool; results go before the wait status."""
    prices = PROMPTS["prices"]
    handles: dict[str, tuple[str, str]] = {}  # task_handle -> (call_id, sku)
    delivered: set[str] = set()
    current = session.send(
        "t1-start",
        input_value=PROMPTS["wait_start"],
        tools=TOOLS["wait_tool"],
        instructions=PROMPTS["instructions"]["wait"],
    )
    session.require(current.ok, f"t1: expected HTTP 200, got {current.status}: {current.error}")
    finished = False
    for turn in range(2, 7):
        for call in current.calls("function_call", "lookup_price"):
            arguments = json.loads(call.get("arguments") or "{}")
            session.expect(call.get("async") is True, "lookup_price call is not marked async")
            handles[arguments["task_handle"]] = (call["call_id"], arguments["sku"])
        waits = current.calls("function_call", "wait_for_tasks")
        for wait in waits:
            session.expect(wait.get("async") is not True, "wait_for_tasks must stay synchronous")
        if not waits:
            pending = [handle for handle in handles if handle not in delivered]
            if not pending:
                finished = True
                break
            requested = pending
        else:
            requested = json.loads(waits[0].get("arguments") or "{}").get("task_handles", [])
        items = []
        for handle in requested:
            if handle in delivered or handle not in handles:
                continue
            call_id, sku = handles[handle]
            items.append(function_output(call_id, json.dumps({"task_handle": handle, "sku": sku, **prices[sku]})))
            delivered.add(handle)
        for wait in waits:
            items.append(
                function_output(
                    wait["call_id"],
                    json.dumps({"status": "completed", "completed_task_handles": requested}),
                )
            )
        session.require(bool(items), f"t{turn}: nothing to deliver but the model is still waiting")
        current = session.send(
            f"t{turn}-deliver-results",
            input_value=items,
            tools=TOOLS["wait_tool"],
            instructions=PROMPTS["instructions"]["wait"],
            previous=current,
        )
        session.require(current.ok, f"t{turn}: delivering results returned {current.status}")
    if not finished and (current.calls("function_call") or len(delivered) < len(handles)):
        session.problems.append("wait-tool: continuation limit reached with work still pending")
    session.require(len(handles) == 2, f"expected two lookup_price calls, got {len(handles)}")
    session.expect("WIDGET" in current.text().upper(), "final answer does not name WIDGET as cheaper")


def scenario_edge_cases(session: Session) -> None:
    """Probes whose status is recorded, not required.

    Every async probe continues the same response (`previous_response_id`), so the recording depends on the
    model making the async call once; each probe is an independent branch of that response.
    """
    instructions = PROMPTS["instructions"]["weather"]
    weather = PROMPTS["weather_output"]

    # Baseline: a synchronous call left unanswered must fail the follow-up.
    first = session.send(
        "baseline-sync-unanswered/t1-start",
        input_value=PROMPTS["weather_start"],
        tools=TOOLS["weather_sync"],
        instructions=instructions,
    )
    session.require(first.ok and first.calls("function_call", "get_weather"), "baseline: no synchronous call")
    session.require(
        first.calls("function_call", "get_weather")[0].get("async") is not True,
        "baseline: synchronous call is marked async",
    )
    session.send(
        "baseline-sync-unanswered/t2-follow-up-without-output",
        input_value=PROMPTS["weather_follow_up_1"],
        tools=TOOLS["weather_sync"],
        instructions=instructions,
        previous=first,
    )

    first, call = start_async_weather(session, "async-call/t1-start")
    session.send(
        "unknown-call-id/t2-output",
        input_value=[function_output("call_async_unknown_probe", weather)],
        tools=TOOLS["weather_async"],
        instructions=instructions,
        previous=first,
    )

    session.send(
        "duplicate-same-request/t2-output-twice",
        input_value=[function_output(call["call_id"], weather), function_output(call["call_id"], weather)],
        tools=TOOLS["weather_async"],
        instructions=instructions,
        previous=first,
    )

    second = session.send(
        "duplicate-across-requests/t2-output",
        input_value=[function_output(call["call_id"], weather)],
        tools=TOOLS["weather_async"],
        instructions=instructions,
        previous=first,
    )
    if second.ok:
        session.send(
            "duplicate-across-requests/t3-output-again",
            input_value=[function_output(call["call_id"], weather)],
            tools=TOOLS["weather_async"],
            instructions=instructions,
            previous=second,
        )
    else:
        session.problems.append("duplicate-across-requests: first delivery failed; repeat not probed")

    session.send(
        "conflicting-same-request/t2-two-different-outputs",
        input_value=[
            function_output(call["call_id"], weather),
            function_output(call["call_id"], PROMPTS["weather_output_conflicting"]),
        ],
        tools=TOOLS["weather_async"],
        instructions=instructions,
        previous=first,
    )

    session.send(
        "redeclared-without-async/t2-follow-up-without-output",
        input_value=PROMPTS["weather_follow_up_1"],
        tools=TOOLS["weather_sync"],
        instructions=instructions,
        previous=first,
    )

    session.send(
        "unsupported-model/t1-start",
        input_value=PROMPTS["weather_start"],
        tools=TOOLS["weather_async"],
        instructions=instructions,
        model=session.unsupported_model,
    )

    session.send(
        "async-hosted-web-search/t1-start",
        input_value="Use web search to find the population of Lisbon. Answer in one sentence.",
        tools=TOOLS["async_hosted_web_search"],
    )


def scenario_parallel_mixed(session: Session) -> None:
    """Parallel async and synchronous calls; answer only the synchronous one, then the async one."""
    first = session.send(
        "t1-start",
        input_value=PROMPTS["parallel_start"],
        tools=TOOLS["parallel_mixed"],
        instructions=PROMPTS["instructions"]["weather"],
        extra={"parallel_tool_calls": True},
    )
    session.require(first.ok, f"t1: expected HTTP 200, got {first.status}: {first.error}")
    weather = first.calls("function_call", "get_weather")
    clock = first.calls("function_call", "get_local_time")
    session.require(len(weather) == 1 and len(clock) == 1, "t1: expected one get_weather and one get_local_time call")
    session.expect(weather[0].get("async") is True, "t1: get_weather call is not marked async")
    session.expect(clock[0].get("async") is not True, "t1: get_local_time call is marked async")
    second = session.send(
        "t2-sync-output-only",
        input_value=[function_output(clock[0]["call_id"], PROMPTS["local_time_output"])],
        tools=TOOLS["parallel_mixed"],
        instructions=PROMPTS["instructions"]["weather"],
        previous=first,
        extra={"parallel_tool_calls": True},
    )
    session.require(second.ok, f"t2: answering only the synchronous call returned {second.status}")
    late = session.send(
        "t3-late-async-output",
        input_value=[function_output(weather[0]["call_id"], PROMPTS["weather_output"]), user(PROMPTS["weather_ask"])],
        tools=TOOLS["parallel_mixed"],
        instructions=PROMPTS["instructions"]["weather"],
        previous=second,
        extra={"parallel_tool_calls": True},
    )
    session.require(late.ok, f"t3: late async output returned {late.status}")


def multi_agent_scenario(parallel: bool) -> Callable[[Session], None]:
    def run(session: Session) -> None:
        """Multi-agent mode with an async tool; the guide says not to combine it with parallel calls."""
        extra = {
            "multi_agent": {"enabled": True, "max_concurrent_subagents": 1},
            "parallel_tool_calls": parallel,
        }
        first = session.send(
            "t1-start",
            input_value=PROMPTS["multi_agent_start"],
            tools=TOOLS["weather_async"],
            instructions=PROMPTS["instructions"]["multi_agent"],
            extra=extra,
            beta=MULTI_AGENT_BETA,
        )
        if not first.ok:
            return  # The rejection itself is the observation.
        calls = [
            item
            for item in first.output
            if item.get("type") == "function_call" and item.get("name") == "get_weather"
        ]
        if not calls:
            session.problems.append("t1: accepted, but no get_weather call to continue")
            return
        session.send(
            "t2-late-output",
            input_value=[function_output(calls[0]["call_id"], PROMPTS["weather_output"]), user(PROMPTS["weather_ask"])],
            tools=TOOLS["weather_async"],
            instructions=PROMPTS["instructions"]["multi_agent"],
            previous=first,
            extra=extra,
            beta=MULTI_AGENT_BETA,
        )

    return run


# ── upstream requests for --sample ───────────────────────────────────────────
#
# The gateway calls the model server with store:false and the full item history, `async` stripped, and the two
# hints added. These helpers build the same requests to measure how an open model continues around a call whose
# output has not arrived, with and without the hints.


def upstream_tools(hinted: bool) -> list:
    tools = copy.deepcopy(TOOLS["weather_sync"])
    if hinted:
        tools[0]["description"] += PROMPTS["upstream_hints"]["tool_description_suffix"]
    return tools


def with_pending_notes(history: list[dict]) -> list[dict]:
    """Insert the pending-call note after every function call that has no output in `history`."""
    answered = {item["call_id"] for item in history if item.get("type") == "function_call_output"}
    upstream = []
    for item in history:
        upstream.append(copy.deepcopy(item))
        if item.get("type") == "function_call" and item["call_id"] not in answered:
            note = PROMPTS["upstream_hints"]["pending_note"].format(name=item["name"], call_id=item["call_id"])
            upstream.append({"type": "message", "role": "developer", "content": note})
    return upstream


def shell_output(call_id: str) -> dict:
    return {
        "type": "shell_call_output",
        "call_id": call_id,
        "output": [{"stdout": "SHELL_OK\n", "stderr": "", "outcome": {"type": "exit", "exit_code": 0}}],
    }


def tool_search_output(call_id: str) -> dict:
    return {
        "type": "tool_search_output",
        "call_id": call_id,
        "execution": "client",
        "status": "completed",
        "tools": copy.deepcopy(TOOLS["weather_sync"]),
    }


def probe_tool_type(
    session: Session,
    name: str,
    *,
    tools: list,
    prompt: str,
    instructions: str,
    call_type: str,
    output: Callable[[str], dict],
) -> None:
    """Declare one client-executed tool type with `async: true`, then probe the pending-call contract.

    The first request's status is the main observation. When it is accepted and the model calls the tool, a
    follow-up without the output shows whether the call is treated as pending (async) or unanswered (sync),
    and a late output shows whether the original call_id is still accepted.
    """
    first = session.send(f"{name}/t1-start", input_value=prompt, tools=tools, instructions=instructions)
    if not first.ok:
        return  # The rejection is the observation.
    calls = first.calls(call_type)
    if not calls:
        session.problems.append(f"{name}: accepted, but the model made no {call_type}")
        return
    follow_up = session.send(
        f"{name}/t2-follow-up-without-output",
        input_value=PROMPTS["weather_follow_up_1"],
        tools=tools,
        instructions=instructions,
        previous=first,
    )
    session.send(
        f"{name}/t3-late-output",
        input_value=[output(calls[0]["call_id"]), user("Summarize the tool result in one sentence.")],
        tools=tools,
        instructions=instructions,
        previous=follow_up if follow_up.ok else first,
    )


def scenario_client_tool_types(session: Session) -> None:
    """`async` on client-executed tool types beyond plain function and custom tools."""
    weather = PROMPTS["instructions"]["weather"]
    probe_tool_type(
        session,
        "namespace-member",
        tools=TOOLS["namespace_member_async"],
        prompt=PROMPTS["weather_start"],
        instructions=weather,
        call_type="function_call",
        output=lambda call_id: function_output(call_id, PROMPTS["weather_output"]),
    )
    probe_tool_type(
        session,
        "namespace-itself",
        tools=TOOLS["namespace_itself_async"],
        prompt=PROMPTS["weather_start"],
        instructions=weather,
        call_type="function_call",
        output=lambda call_id: function_output(call_id, PROMPTS["weather_output"]),
    )
    probe_tool_type(
        session,
        "shell",
        tools=TOOLS["shell_async"],
        prompt=PROMPTS["shell_start"],
        instructions=PROMPTS["instructions"]["shell"],
        call_type="shell_call",
        output=shell_output,
    )
    probe_tool_type(
        session,
        "tool-search-client",
        tools=TOOLS["tool_search_client_async"],
        prompt=PROMPTS["tool_search_start"],
        instructions=PROMPTS["instructions"]["tool_search"],
        call_type="tool_search_call",
        output=tool_search_output,
    )


NONSTREAMING = (False,)
STREAMING = (True,)
BOTH = (False, True)

# Each scenario records only the modes that answer a distinct question; see async_tools/README.md.
SCENARIOS: dict[str, tuple[Callable[[Session], None], tuple[bool, ...]]] = {
    "function-delayed-result": (scenario_function_delayed_result, BOTH),
    "custom-delayed-result": (scenario_custom_delayed_result, NONSTREAMING),
    "web-search-mixed": (scenario_web_search_mixed, STREAMING),
    "wait-tool": (scenario_wait_tool, NONSTREAMING),
    "edge-cases": (scenario_edge_cases, NONSTREAMING),
    "client-tool-types": (scenario_client_tool_types, NONSTREAMING),
    "parallel-mixed": (scenario_parallel_mixed, NONSTREAMING),
    "multi-agent-parallel": (multi_agent_scenario(parallel=True), NONSTREAMING),
    "multi-agent-sequential": (multi_agent_scenario(parallel=False), NONSTREAMING),
}

# ── continuation sampling (vLLM, not recorded) ───────────────────────────────

TEMPERATURE = re.compile(r"-?\d+\s*(°|º|degrees)", re.IGNORECASE)


def classify_continuation(client: httpx.Client, url: str, model: str, hinted: bool, headers: dict) -> str:
    """One t1 + continuation round with the call pending: ok, re-called, fabricated, no-call, or error."""
    base = {
        "model": model,
        "instructions": PROMPTS["instructions"]["weather"],
        "store": False,
        "max_output_tokens": VLLM_MAX_OUTPUT_TOKENS,
        "tools": upstream_tools(hinted),
    }
    history = [user(PROMPTS["weather_start"])]
    first = client.post(url, json=base | {"input": history}, headers=headers, timeout=READ_TIMEOUT)
    if first.status_code != 200:
        return "error"
    output = first.json().get("output") or []
    if not any(item.get("type") == "function_call" for item in output):
        return "no-call"
    history += output
    upstream = with_pending_notes(history) if hinted else history
    second = client.post(url, json=base | {"input": upstream}, headers=headers, timeout=READ_TIMEOUT)
    if second.status_code != 200:
        return "error"
    exchange = Exchange(200, second.json())
    if exchange.calls("function_call"):
        return "re-called"
    return "fabricated" if TEMPERATURE.search(exchange.text()) else "ok"


def sample_continuations(base_url: str, model: str, samples: int, api_key: str | None) -> None:
    """Print how often the model fabricates, re-calls, or continues correctly, with and without hints."""
    url = f"{base_url.rstrip('/')}/v1/responses"
    headers = {"Authorization": f"Bearer {api_key}"} if api_key else {}
    with httpx.Client() as client, ThreadPoolExecutor(3) as pool:
        for hinted in (False, True):
            outcomes = list(
                pool.map(
                    lambda flag: classify_continuation(client, url, model, flag, headers), [hinted] * samples
                )
            )
            counts = {kind: outcomes.count(kind) for kind in ("ok", "re-called", "fabricated", "no-call", "error")}
            print(f"{'hinted' if hinted else 'unhinted':9} {counts}", flush=True)


# ── recording ────────────────────────────────────────────────────────────────


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def slug(value: str) -> str:
    return value.replace("/", "-").replace(":", "-").replace(" ", "-")


def record(
    scenario: str,
    *,
    provider: str,
    base_url: str,
    model: str,
    unsupported_model: str,
    stream: bool,
    output_dir: Path,
    api_key: str | None,
) -> bool:
    mode = "streaming" if stream else "nonstreaming"
    final = output_dir / f"async-tool-{provider}-{scenario}-{slug(model)}-{mode}.yaml"
    handle, staged_name = tempfile.mkstemp(prefix=f".{final.stem}.", suffix=".yaml", dir=output_dir)
    os.close(handle)
    staged = Path(staged_name)
    print(f"\n[{provider}] {scenario} ({mode}) -> {final.name}", flush=True)
    port = free_port()
    # The proxy reads this module global when it starts; without it a slow request is cut at 300 s.
    record_cassette.HTTP_READ_TIMEOUT = READ_TIMEOUT
    proxy = _start_proxy(staged, base_url, port)
    headers = {"Authorization": f"Bearer {api_key}"} if api_key else {}
    session = Session(
        httpx.Client(),
        f"http://127.0.0.1:{port}",
        model,
        stream,
        scenario,
        headers,
        unsupported_model,
        VLLM_MAX_OUTPUT_TOKENS if provider in ("vllm", "gateway") else MAX_OUTPUT_TOKENS,
    )
    try:
        SCENARIOS[scenario][0](session)
    except ScenarioAbort as abort:
        session.problems.append(f"aborted: {abort}")
    except Exception as error:  # noqa: BLE001 - keep the partial recording and continue other scenarios
        session.problems.append(f"aborted: {type(error).__name__}: {error}")
    finally:
        session.client.close()
        _stop_proxy(proxy)
    assert_masked(staged)
    fatal = [problem for problem in session.problems if problem.startswith("aborted:")]
    if fatal:
        kept = staged.with_name(f"{final.stem}.failed.yaml")
        staged.replace(kept)
        print(f"  FAILED: {'; '.join(fatal)}\n  Recording kept for inspection: {kept}", flush=True)
        return False
    staged.replace(final)
    for problem in session.problems:
        print(f"  note: {problem}", flush=True)
    print(f"  recorded {final}", flush=True)
    return True


def assert_masked(path: Path) -> None:
    document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    for turn in document.get("turns", []):
        authorization = (turn.get("request", {}).get("headers") or {}).get("authorization")
        if authorization not in (None, "Bearer ***", "***"):
            path.unlink()
            raise SystemExit(f"refusing to keep {path.name}: authorization header was not masked")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--provider",
        choices=["openai-reference", "gateway", "vllm"],
        default="openai-reference",
        help="vllm records nothing: with --sample it measures continuations with and without the hints",
    )
    parser.add_argument("--base-url", default=None, help="Default: https://api.openai.com; required otherwise")
    parser.add_argument("--model", default=None, help=f"Default for OpenAI: {DEFAULT_OPENAI_MODEL}")
    parser.add_argument(
        "--unsupported-model",
        default=None,
        help=f"Model for the unsupported-model probe. Default: {DEFAULT_UNSUPPORTED_MODEL} for OpenAI; the served "
        "model for the gateway, which accepts async for every model",
    )
    parser.add_argument("--scenario", choices=["all", *SCENARIOS], default="all")
    parser.add_argument(
        "--stream-mode",
        choices=["declared", "both", "streaming", "nonstreaming"],
        default="declared",
        help="declared (default) records each scenario's own modes; the others override them",
    )
    parser.add_argument("--output-dir", type=Path, default=FIXTURES_DIR)
    parser.add_argument("--dry-run", action="store_true", help="List recordings without contacting any API")
    parser.add_argument(
        "--sample",
        type=int,
        default=0,
        metavar="N",
        help="vllm only: run N unrecorded continuation rounds per hint mode and print outcome counts",
    )
    args = parser.parse_args()

    if args.provider == "openai-reference":
        base_url = args.base_url or "https://api.openai.com"
        model = args.model or DEFAULT_OPENAI_MODEL
        api_key = os.environ.get("OPENAI_API_KEY")
        if not api_key and not args.dry_run:
            print("ERROR: export OPENAI_API_KEY before recording OpenAI references", file=sys.stderr)
            return 2
    else:
        if not args.base_url or not args.model:
            print(f"ERROR: {args.provider} recordings need --base-url and --model", file=sys.stderr)
            return 2
        key_variable = "GATEWAY_API_KEY" if args.provider == "gateway" else "VLLM_API_KEY"
        base_url, model, api_key = args.base_url, args.model, os.environ.get(key_variable)

    if args.provider == "vllm" and not args.sample:
        print("ERROR: --provider vllm only samples continuations; pass --sample N", file=sys.stderr)
        return 2
    available = SCENARIOS
    if args.sample:
        if args.provider != "vllm":
            print("ERROR: --sample is only available with --provider vllm", file=sys.stderr)
            return 2
        sample_continuations(base_url, model, args.sample, api_key)
        return 0

    scenarios = list(available) if args.scenario == "all" else [args.scenario]
    overrides = {"both": BOTH, "streaming": STREAMING, "nonstreaming": NONSTREAMING}

    def modes_for(scenario: str) -> tuple[bool, ...]:
        return overrides.get(args.stream_mode, available[scenario][1])

    if args.dry_run:
        for scenario in scenarios:
            for stream in modes_for(scenario):
                mode = "streaming" if stream else "nonstreaming"
                print(f"async-tool-{args.provider}-{scenario}-{slug(model)}-{mode}.yaml")
        return 0

    args.output_dir.mkdir(parents=True, exist_ok=True)
    failures = []
    for scenario in scenarios:
        for stream in modes_for(scenario):
            ok = record(
                scenario,
                provider=args.provider,
                base_url=base_url,
                model=model,
                unsupported_model=args.unsupported_model
                or (model if args.provider == "gateway" else DEFAULT_UNSUPPORTED_MODEL),
                stream=stream,
                output_dir=args.output_dir,
                api_key=api_key,
            )
            if not ok:
                failures.append(f"{scenario} ({'streaming' if stream else 'nonstreaming'})")
    if failures:
        print("\nFailed recordings:\n  " + "\n  ".join(failures), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
