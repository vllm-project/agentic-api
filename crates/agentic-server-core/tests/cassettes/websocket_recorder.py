"""Persistent duplex recording and bounded edge probes; no synthesized server events."""
from __future__ import annotations

import argparse
import base64
from collections import Counter
import math
import select
import signal
import socket
import json
import os
import struct
import sys
import time
from pathlib import Path
from typing import Any, Callable

import yaml


class SessionRecorder:
    """Write observations immediately so failed/interrupted runs retain evidence."""

    def __init__(self, output: Path) -> None:
        self.output = output
        self.started = time.monotonic()
        self.sequence = 0
        self.bytes = 0
        self.file = None
        self.capture_started = False

    def start_session(self, handshake: dict) -> None:
        if self.file is not None:
            raise RuntimeError("finish the current recording session before starting another")
        self.output.parent.mkdir(parents=True, exist_ok=True)
        self.file = self.output.open("a" if self.capture_started else "w", encoding="utf-8")
        if not self.capture_started:
            self.file.write("format: responses-websocket-v1\nsessions:\n")
            self.capture_started = True
        self.started = time.monotonic()
        self.sequence = 0
        self.bytes = 0
        self.file.write("  - handshake:\n")
        self._write(handshake, 6)
        self.file.write("    frames:\n")
        self.file.flush()

    def _write(self, value: Any, indent: int) -> None:
        text = yaml.safe_dump(value, allow_unicode=True, sort_keys=False, width=120)
        self.file.write("".join(" " * indent + line for line in text.splitlines(True)))
        self.file.flush()

    def record_frame(self, direction: str, opcode: int, payload: bytes, fin: bool) -> None:
        self.bytes += len(payload)
        # Both bounds are recording budgets, not assertions about provider behavior.
        if self.sequence >= 250_000 or self.bytes > 128 * 1024 * 1024:
            raise RuntimeError("WebSocket capture exceeded its frame/byte budget")
        frame = {
            "ordinal": self.sequence,
            "elapsed_ms": round((time.monotonic() - self.started) * 1000, 3),
            "direction": direction, "opcode": opcode, "fin": fin,
        }
        if opcode == 1:
            try:
                frame["text"] = payload.decode("utf-8")
            except UnicodeDecodeError:
                frame["payload_base64"] = base64.b64encode(payload).decode("ascii")
        else:
            frame["payload_base64"] = base64.b64encode(payload).decode("ascii")
        if opcode == 8 and len(payload) >= 2:
            frame["close_code"] = struct.unpack("!H", payload[:2])[0]
            frame["close_reason"] = payload[2:].decode("utf-8", errors="replace")
        self.sequence += 1
        self._write([frame], 6)

    def finish_session(self, close_outcome: dict) -> None:
        if self.file is not None:
            self.file.write("    close_outcome:\n")
            self._write(close_outcome, 6)
            self.file.close()
            self.file = None


class RecordedSession:
    @staticmethod
    def responses_url(base_url: str) -> str:
        from urllib.parse import urlparse

        parsed = urlparse(base_url.rstrip("/"))
        if parsed.scheme in {"ws", "wss"}:
            root = base_url.rstrip("/")
        elif parsed.scheme == "http":
            root = "ws://" + base_url.rstrip("/")[len("http://"):]
        elif parsed.scheme == "https":
            root = "wss://" + base_url.rstrip("/")[len("https://"):]
        else:
            raise ValueError(f"unsupported websocket base URL: {base_url}")
        return f"{root}/v1/responses"

    def __init__(
        self, ws: Any, output: Path, handshake: dict, timeout: float,
        *, session_recorder: SessionRecorder | None = None,
    ) -> None:
        self.ws = ws
        self.recorder = session_recorder if session_recorder is not None else SessionRecorder(output)
        self.handshake = handshake
        self.timeout = timeout
        self.peer_closed = False
        self.probe_result = None

    def __enter__(self) -> RecordedSession:
        try:
            self.ws.__enter__()
        except BaseException as error:
            self.handshake["response"] = self.ws.handshake_response
            self.recorder.start_session(self.handshake)
            self.recorder.finish_session({"kind": "handshake_failure", "error": str(error)})
            if self.ws.sock is not None:
                self.ws.sock.close()
            raise
        self.handshake["response"] = self.ws.handshake_response
        self.recorder.start_session(self.handshake)
        self.ws.on_frame = self.recorder.record_frame
        self.ws.sock.settimeout(self.timeout)
        return self

    def send_client_event(self, event: dict) -> None:
        self.ws.send_text(json.dumps(event, separators=(",", ":"), ensure_ascii=False))

    def record_server_frame(self) -> dict:
        text = self.ws.receive_text()
        if text is None:
            self.peer_closed = True
            raise RuntimeError("server closed before all response/injection outcomes arrived")
        event = json.loads(text)
        if not isinstance(event, dict):
            raise ValueError("server message is not a JSON object")
        return event

    def __exit__(self, error_type: Any, error: Any, traceback: Any) -> None:
        outcome = {"kind": "completed" if error is None else "driver_or_transport_failure"}
        if error is not None:
            outcome["error"] = str(error)
        if self.probe_result is not None:
            outcome["probe"] = self.probe_result
        try:
            if not self.peer_closed:
                self.ws.send_close()
                self.ws.sock.settimeout(5)
                # Preserve any late server frames observed during the close handshake.
                while self.ws.receive_text() is not None:
                    pass
                self.peer_closed = True
            outcome["peer_close_received"] = self.peer_closed
        except (OSError, EOFError, RuntimeError) as close_error:
            outcome["peer_close_received"] = False
            outcome["close_error"] = str(close_error)
        finally:
            self.ws.sock.close()
            self.recorder.finish_session(outcome)


CLIENT_CALLS = {"function_call", "shell_call", "custom_tool_call", "tool_search_call"}
TERMINALS = {"response.completed", "response.failed", "response.incomplete"}


def exchange(
    session: RecordedSession, body: dict, build_outputs: Callable[[list[dict]], list[dict]],
    inject: bool, max_injections: int = 100,
) -> tuple[dict, list[dict]]:
    """Receive while injecting; finish only after terminal AND every admitted ack."""
    wire = dict(body, type="response.create")
    wire.pop("stream", None)
    session.send_client_event(wire)
    response_id = None
    terminal = None
    queued = []
    inflight = None
    calls_seen = set()
    accepted = set()
    fallback = []
    injection_count = 0
    while terminal is None or inflight is not None or queued:
        event = session.record_server_frame()
        kind = event.get("type")
        if kind == "error":
            raise RuntimeError(f"server error: {json.dumps(event, ensure_ascii=False)}")
        if kind == "response.created":
            response_id = event["response"]["id"]
        elif kind == "response.output_item.done" and inject:
            item = event.get("item", {})
            call_id = item.get("call_id")
            if item.get("type") in CLIENT_CALLS and call_id not in calls_seen:
                calls_seen.add(call_id)
                outputs = build_outputs([item])
                if not outputs:
                    raise RuntimeError(f"no output fixture for client call {call_id}")
                queued.extend(outputs)
                if len(queued) > 64:
                    raise RuntimeError("client output queue exceeded 64 entries")
        elif kind in {"response.inject.created", "response.inject.failed"}:
            if inflight is None or event.get("response_id") != response_id:
                raise RuntimeError("unexpected injection acknowledgement")
            if kind == "response.inject.created":
                accepted.update(item["call_id"] for item in inflight)
            elif event.get("error", {}).get("code") == "response_already_completed":
                # Only the explicitly returned, uncommitted input is retried.
                returned = event.get("input")
                if not isinstance(returned, list):
                    raise RuntimeError("completed-response rejection omitted returned input")
                fallback.extend(returned)
            else:
                raise RuntimeError(f"injection rejected: {json.dumps(event, ensure_ascii=False)}")
            inflight = None
        elif kind in TERMINALS:
            terminal = event["response"]
            response_id = terminal["id"]
        if queued and inflight is None:
            if not response_id:
                raise RuntimeError("tool call arrived without a response identity")
            if injection_count >= max_injections:
                raise RuntimeError("injection count exceeded recording budget")
            inflight, queued = queued, []
            injection_count += 1
            session.send_client_event({"type": "response.inject", "response_id": response_id, "input": inflight})
    if terminal.get("status") != "completed":
        raise RuntimeError(f"response ended with status {terminal.get('status')}")
    # Prevent run_responses from replaying calls already accepted in this session.
    # This is driver bookkeeping only; the captured terminal frame stays untouched.
    remaining = [
        item for item in terminal.get("output", [])
        if item.get("type") in CLIENT_CALLS and item.get("call_id") not in accepted
    ]
    if not inject:
        fallback.extend(build_outputs(remaining))
    return terminal, fallback


# Edge probes record provider errors as evidence; uncertain input is never retried.
CASES = (
    "late-continuation", "duplicate-in-batch", "mixed-valid-invalid", "duplicate-injection",
)
ACKS = {"response.inject.created", "response.inject.failed"}
TOOL = {"type": "function", "name": "edge_echo", "description": "Return the fixture marker.",
        "parameters": {"type": "object", "properties": {}, "required": [], "additionalProperties": False}}


def load_prompt(name: str) -> str:
    source = Path(__file__).with_name("multi_agent") / "prompts.txt"
    _, marker, section = source.read_text(encoding="utf-8").partition(f"[{name}]")
    prompt = section.split("\n[", 1)[0].strip() if marker else ""
    if not prompt:
        raise ValueError(f"missing [{name}] prompt")
    return prompt


def completed_cases(output: Path) -> set[str]:
    """Keep every attempt; skip cases with a completed, conclusive observation."""
    if not output.exists():
        return set()
    if output.stat().st_size > 128 * 1024 * 1024:
        raise ValueError("edge capture exceeds 128 MiB; archive it before recording more attempts")
    capture = yaml.safe_load(output.read_text(encoding="utf-8"))
    if capture.get("format") != "responses-websocket-v1":
        raise ValueError("unexpected capture format")
    completed = set()
    for session in capture.get("sessions", []):
        close = session.get("close_outcome") or {}
        if not close:
            raise ValueError("unfinished session in capture; archive it before retrying")
        case = session["handshake"]["probe"]["case"]
        probe = close.get("probe") or {}
        if (close.get("kind") == "completed" and close.get("peer_close_received")
                and (case == "late-continuation" or probe.get("active_validation_observed"))):
            completed.add(case)
    return completed


class PeerClosed(Exception):
    """Observed close frame; distinct from EOF, timeout, or socket failure."""


class Probe:
    def __init__(self, session, model: str, timeout: float, active: bool = False):
        self.session = session
        self.model = model
        self.deadline = time.monotonic() + timeout
        self.response_id = None
        self.terminal = None
        self.call = None
        self.active = active
        self.sibling_streaming = False
        self.sibling_finished = False
        self.observed_outcomes = []
        self.session.probe_result = {
            "active_requested": active, "sibling_streaming_observed": False,
            "injected_during_sibling_execution": False, "control_outcomes": self.observed_outcomes,
            "active_validation_observed": False,
        }

    def send(self, event):
        self.session.send_client_event(event)

    def receive(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("probe wall-clock budget exhausted")
        self.session.ws.sock.settimeout(remaining)
        try:
            event = self.session.record_server_frame()
        except RuntimeError:
            if self.session.peer_closed:
                raise PeerClosed from None
            raise
        kind = event.get("type")
        agent = event.get("agent", {}).get("agent_name", "")
        if agent.startswith("/root/") and kind == "response.output_text.delta":
            self.sibling_streaming = True
            self.session.probe_result["sibling_streaming_observed"] = True
        item = event.get("item", {})
        item_agent = item.get("agent", {}).get("agent_name", agent)
        if (item_agent.startswith("/root/") and kind == "response.output_item.done"
                and item.get("type") == "message" and item.get("phase") == "final_answer"):
            self.sibling_finished = True
        if kind in ACKS or kind == "error":
            if (self.session.probe_result["injected_during_sibling_execution"]
                    and event.get("error", {}).get("code") != "response_already_completed"
                    and self.terminal is None):
                self.session.probe_result["active_validation_observed"] = True
            self.observed_outcomes.append({
                "type": kind, "code": event.get("error", {}).get("code"),
                "terminal_already_observed": self.terminal is not None,
            })
        if kind == "response.created":
            self.response_id = event["response"]["id"]
        if kind in TERMINALS:
            self.terminal = event
        if kind == "response.output_item.done" and event.get("item", {}).get("type") == "function_call":
            if self.call is not None:
                raise RuntimeError("probe prerequisite failed: model produced more than one client call")
            self.call = event["item"]
        return event

    def until(self, kinds):
        while True:
            event = self.receive()
            if event.get("type") == "error":
                # Observe whether the server closes; never synthesize a close or retry.
                self.deadline = min(self.deadline, time.monotonic() + 5)
                while True:
                    self.receive()
            if event.get("type") in kinds:
                return event

    def create(self, tool=False, **overrides):
        body = {"type": "response.create", "model": self.model, "store": True,
                "multi_agent": {"enabled": True, "max_concurrent_subagents": 1},
                "max_output_tokens": 2048, "input": load_prompt("websocket-edge") if tool else "Reply with EDGE_OK only."}
        if tool:
            body["tools"] = [TOOL]
        if self.active:
            body["input"] = load_prompt("websocket-active")
            body["max_output_tokens"] = 16384
            body["tools"] = [TOOL]
        body.update(overrides)
        self.response_id = self.terminal = self.call = None
        self.send(body)

    def injection(self, items, response_id=None):
        return {"type": "response.inject", "response_id": response_id or self.response_id, "input": items}

    def finish(self):
        if self.terminal is None:
            self.until(TERMINALS)

    def run(self, case):
        if case == "late-continuation":
            self.active = False  # The late probe intentionally waits for quiescence.
        fake = {"type": "function_call_output", "call_id": "call_edge_unknown", "output": "EDGE_OK"}
        self.create(tool=True)
        while self.call is None:
            self.until({"response.output_item.done"} | TERMINALS)
            if self.terminal is not None and self.call is None:
                raise RuntimeError("probe prerequisite failed: model did not call edge_echo")
        good = {"type": "function_call_output", "call_id": self.call["call_id"], "output": "EDGE_OK"}
        if case == "late-continuation":
            self.finish()
            self.send(self.injection([good]))
            reply = self.until(ACKS)
            if reply.get("error", {}).get("code") == "response_already_completed":
                returned = reply.get("input")
                if not isinstance(returned, list):
                    raise RuntimeError("late rejection omitted returned input")
                previous = self.response_id
                self.create(tool=True, input=returned, previous_response_id=previous)
                self.finish()
            return
        if self.active:
            # Require fresh sibling output after observing the pending call.
            self.sibling_streaming = False
            while not self.sibling_streaming and not self.sibling_finished and self.terminal is None:
                self.until({"response.output_text.delta"} | TERMINALS)
            if self.terminal is not None or self.sibling_finished:
                raise RuntimeError("active probe inconclusive: sibling was not executing before injection")
            self.session.probe_result["injected_during_sibling_execution"] = True
        batch = {
            "duplicate-in-batch": [good, dict(good)],
            "mixed-valid-invalid": [good, fake], "duplicate-injection": [good],
        }[case]
        self.send(self.injection(batch))
        if case == "duplicate-injection":
            # Deliberate duplicate, not a retry following an uncertain outcome.
            self.send(self.injection(batch))
        reply = self.until(ACKS)
        if case == "duplicate-injection":
            self.until(ACKS)
            self.finish()
            return
        if reply["type"] == "response.inject.failed" and reply.get("error", {}).get("code") != "response_already_completed":
            # The batch was explicitly rejected. Submitting the good output tests
            # whether a mixed batch committed any prefix before rejecting it.
            self.send(self.injection([good]))
            self.until(ACKS)
        self.finish()


def main():
    # Import the shared socket client only for the CLI; regular recordings import
    # this module from record_cassette.py.
    import record_cassette as recorder

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", choices=["openai-reference", "gateway"], required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--case", choices=["all", *CASES], default="all")
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--active", action="store_true", help="Keep sibling text generation active during injection")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    headers = {"OpenAI-Beta": "responses_multi_agent=v1"}
    key = os.environ.get("OPENAI_API_KEY" if args.provider == "openai-reference" else "GATEWAY_API_KEY", "")
    if args.provider == "openai-reference" and not key and not args.dry_run:
        parser.error("export OPENAI_API_KEY before recording")
    if key:
        headers["Authorization"] = f"Bearer {key}"
    url = recorder._websocket_url(args.url)
    slug = args.model.translate(str.maketrans("/: ", "---"))
    failed = []
    selection = "active-text-edge-cases" if args.active else "edge-cases"
    if not args.active and args.case != "all":
        selection = f"edge-{args.case}"
    output = args.output_dir / f"multi-agent-{args.provider}-ws-{selection}-{slug}-websocket.yaml"
    done = completed_cases(output) if args.active else set()
    capture = SessionRecorder(output)
    capture.capture_started = args.active and output.exists()
    for case in CASES if args.case == "all" else [args.case]:
        if case in done:
            print(f"Keeping completed {case}: {output}", flush=True)
            continue
        print(f"{'Would record' if args.dry_run else 'Recording'} {case}: {output}", flush=True)
        if args.dry_run:
            continue
        handshake = {"request": {"method": "GET", "url": url, "headers": recorder._filter_request_headers(headers)},
                     "probe": {"case": case, "active": args.active and case != "late-continuation", "wall_timeout_seconds": args.timeout}}
        try:
            with RecordedSession(
                recorder.WebSocketClient(url, headers), output, handshake, args.timeout,
                session_recorder=capture,
            ) as session:
                try:
                    Probe(session, args.model, args.timeout, active=args.active and case != "late-continuation").run(case)
                except PeerClosed:
                    print("Observed server close; inspect captured error and close frames.", flush=True)
                if args.active and case != "late-continuation" and not session.probe_result["active_validation_observed"]:
                    print(f"INCONCLUSIVE active validation for {case}: inspect probe metadata and retry this case.", flush=True)
                    failed.append(case)
        except (OSError, RuntimeError, ValueError, EOFError) as error:
            print(f"Incomplete probe {case}: {error}", flush=True)
            failed.append(case)
    if failed:
        raise SystemExit("Incomplete probes (captures retained): " + ", ".join(failed))


def record_dependencies():
    """Run the existing HTTP recorder as a gateway model-dependency proxy."""
    import record_cassette as recorder
    import uvicorn
    from urllib.parse import urlparse

    parser = argparse.ArgumentParser(description=record_dependencies.__doc__)
    parser.add_argument("--upstream", required=True, help="Actual model server base URL")
    parser.add_argument("--output", type=Path, required=True, help="New dependency YAML; replaced on startup")
    parser.add_argument("--port", type=int, default=7071)
    args = parser.parse_args(sys.argv[2:])
    parsed = urlparse(args.upstream)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc or parsed.username or parsed.password:
        parser.error("--upstream must be an HTTP(S) base URL without credentials")
    if not 1 <= args.port <= 65535:
        parser.error("--port must be between 1 and 65535")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("turns: []\n", encoding="utf-8")
    app = recorder.proxy_app
    app.state.target_host = args.upstream.rstrip("/")
    app.state.output_file = args.output
    app.state.dependency_sequence = 0

    @app.middleware("http")
    async def assign_dependency_ordinal(request, call_next):
        # Allocate at admission, not completion: concurrent requests need unique IDs.
        app.state.dependency_sequence += 1
        request.state.recording_turn = app.state.dependency_sequence
        return await call_next(request)

    print(f"Model dependency proxy: http://127.0.0.1:{args.port}; output: {args.output}", flush=True)
    uvicorn.run(app, host="127.0.0.1", port=args.port, log_level="warning")


# Mid-turn steering uses the same duplex capture and tool exchange above.
from multi_turn_steering.fixtures import (
    STEERING_MIXED_CASES, STEERING_CATEGORIES, steering_prompt,
    steering_request_settings as _steering_request_settings,
    steering_tool_outputs as _steering_tool_outputs,
)


def steering_request_settings(case: str) -> dict:
    from record_cassette import _load_tools_file

    return _steering_request_settings(case, load_tools=_load_tools_file)


def steering_tool_outputs(case: str, calls: list[dict]) -> list[dict]:
    from record_cassette import _load_tools_file, _load_tool_outputs_file, _build_tool_output_input

    return _steering_tool_outputs(
        case, calls, load_tools=_load_tools_file, load_outputs=_load_tool_outputs_file,
        build_outputs=_build_tool_output_input,
    )

STEERING_URL = "wss://api.openai.com/v1/responses"
STEERING_GUIDE = "https://developers.openai.com/api/docs/guides/steering"
STEERING_TERMINALS = {"response.completed", "response.incomplete", "response.failed"}
STEERING_SMOKE = ("mixed-tools-steer",)
STEERING_CASES = {
    "client-tools-steer": "Steer with client search, shell, custom and discovered function outputs withheld",
    "three-turn-steer": "Start a task, steer the initial response, then add a third user input to its automatic successor",
    "steer-delta": "Steer after the first output_text.delta; retain emitted output",
    "array-user": "Steer with a user-message array containing input_text",
    "store-false": "Automatic continuation with store:false on the live connection",
    "late-completed": "Steer after the original response has completed",
    "burst": "Send three distinct steers without awaiting acknowledgements",
    "unknown-response": "Target an ID never created on this connection",
    "missing-target": "Omit previous_response_id",
    "empty-input": "Send an empty input string",
    "empty-array": "Send an empty input array",
    "assistant-message": "Send an assistant message instead of a user message",
    "tool-output-input": "Send function_call_output as steering input (guide/schema ambiguity)",
    "extra-field": "Add instructions to the otherwise valid steering event",
    "stream-id-field": "Add stream_id instead of inferring the target lane",
    "unsupported-model": "Run the created-event probe against GPT-5.6",
    "pending-tool": "Queue steer behind a function call; return its actual output after pending",
    "pending-withheld": "Observe pending steering while withholding function output",
    "pending-wrong-output": "Submit an unknown call_id after pending",
    "pending-partial-outputs": "Submit one of two required outputs, then recover with all actual outputs on the original parent",
    "settings-explicit-defaults": "Consume pending steering with a minimal explicit request to observe defaults",
    "pending-output-recovery-store-false": "Recover from invalid output against the original store:false parent",
    "lane-parent-mismatch": "Return pending outputs on another lane, then on the original lane",
    "array-image-user": "Steer using inline image content in a user message",
    "array-multiple-user": "Steer with two ordered user messages; retain each message's distinct data and marker",
    "pending-output-recovery": "Reject an unknown call_id, then return the actual output against the same original parent",
    "old-parent-before-successor": "Steer the original ID after its terminal event, before observing successor creation",
    "old-parent-after-successor": "Steer the original ID immediately after its automatic successor is created",
    "settings-automatic": "Observe automatic successor inheritance of explicit parent settings",
    "settings-explicit": "Observe explicit tool continuation settings while consuming accepted steering",
    "queue-pressure": "Hold a client tool call and send a bounded batch of distinct steers",
    "reconnect-target": "Try steering a completed response ID on a fresh connection",
    "disconnect-pending": "Disconnect with queued steering; reconnect and return actual tool output",
    "hosted-tool-running": "Steer after hosted code interpreter reports interpreting",
    "pending-user-steer": "Steer after a completed function call, then steer again while its output is withheld",
    "pending-steer-then-output": "Queue two updates behind a completed function call, observe waiting, then return output",
}
STEERING_CASES.update({case: "Original mixed-tool scenario with two event-triggered steering updates"
              for case in STEERING_MIXED_CASES})
STEERING_MIRRORED_CASES = set(STEERING_MIXED_CASES)
STEERING_EXTENDED_CASES = {"pending-output-recovery", "old-parent-before-successor",
                           "old-parent-after-successor", "settings-automatic", "settings-explicit"}
STEERING_PENDING_MATRIX_CASES = {"pending-partial-outputs", "settings-explicit-defaults",
                                 "pending-output-recovery-store-false", "lane-parent-mismatch"}
STEERING_EDGE_CASES = tuple(case for case in STEERING_CASES if case not in STEERING_SMOKE)
STEERING_POST_TERMINAL_TOOL_CASES = {"pending-user-steer", "pending-steer-then-output"}
STEERING_HOSTED_TOOL_CASES = {"hosted-tool-running"}
STEERING_ADVANCED_CASES = STEERING_POST_TERMINAL_TOOL_CASES | STEERING_MIRRORED_CASES | STEERING_HOSTED_TOOL_CASES | {"client-tools-steer"}
STEERING_SUITES = {"smoke", "edges", "all", "non-vision", "vision", "client-tools", "input-lifecycle", "tools-settings", "connections"}
STEERING_GROUPS = {"edge-cases", *("suite-" + suite for suite in STEERING_SUITES), *STEERING_CATEGORIES}
STEERING_TOOL_CASES = {"pending-tool", "pending-withheld", "pending-wrong-output", "queue-pressure", "disconnect-pending"} | STEERING_POST_TERMINAL_TOOL_CASES
STEERING_TOOL = {
    "type": "function", "name": "get_project_status",
    "parameters": {"type": "object", "properties": {}, "required": [], "additionalProperties": False},
    "strict": True,
}


class SteeringPrerequisiteMissing(RuntimeError):
    pass


class SteeringBudgetExpired(TimeoutError):
    pass


def steering_expire(_signum, _frame):
    raise SteeringBudgetExpired("scenario wall-clock budget exhausted")


def steering_sanitize(value, key):
    """Shared capture masks auth headers; also redact accidental credential echoes."""
    if isinstance(value, str):
        return value.replace(key, "***") if key else value
    if isinstance(value, dict):
        return {name: steering_sanitize(item, key) for name, item in value.items()}
    if isinstance(value, list):
        return [steering_sanitize(item, key) for item in value]
    return value


class SteeringProbe:
    def __init__(self, session, args, case, findings):
        self.session, self.args, self.case, self.findings = session, args, case, findings
        self.created, self.controls, self.calls = [], [], []
        self.terminals, self.steer_outcomes = {}, {}
        self.steer_submissions = []
        self.sent = 0
        self.injected = self.returned_output = self.awaiting_explicit_response = False
        self.pending_seen = self.generic_error = self.stop = False
        self.item_agents = {}
        self.output_release_at = None
        self.output_wait_started = None
        self.events = Counter()
        findings.update(created=self.created, terminals=self.terminals, controls=self.controls,
                        submissions=[], event_counts={}, steer_outcomes=self.steer_outcomes,
                        steering_submissions=self.steer_submissions, trigger_reached=False,
                        hosted_tools={})

    def send(self, event, trigger):
        self.findings["submissions"].append({
            "trigger": trigger, "type": event["type"], "previous_response_id": event.get("previous_response_id"),
            "frame_ordinal_before_send": self.session.recorder.sequence,
        })
        self.session.send_client_event(event)

    def create(self, **overrides):
        body = {"type": "response.create", "model": self.args.model, "store": self.case != "store-false",
                "max_output_tokens": self.args.max_output_tokens, "input": steering_prompt("plan-request"),
                "instructions": steering_prompt("ws-instructions")}
        if self.case == "unsupported-model":
            body["model"] = self.args.unsupported_model
        if self.case in STEERING_HOSTED_TOOL_CASES:
            body.update(tools=[{"type": "code_interpreter", "container": {"type": "auto"}}],
                        tool_choice="auto",
                        input=steering_prompt("ws-" + self.case))
        if self.case in STEERING_TOOL_CASES and not overrides:
            body.update(tools=[dict(STEERING_TOOL, description=steering_prompt("function-description"))], tool_choice="auto",
                        parallel_tool_calls=False, input=steering_prompt("function-request"))
        if self.case == "three-turn-steer":
            body["input"] = steering_prompt("three-turn-steer")
        if self.case in {"late-completed", "reconnect-target"}:
            body["input"] = steering_prompt("baseline")
        body.update(overrides)
        self.send(body, "initial" if not self.created else "explicit-tool-continuation")

    def steer(self, target, trigger, input_text=None):
        default_key = f"three-turn-steer-update-{self.sent + 1}" if self.case == "three-turn-steer" else "steer"
        steer_text = input_text if input_text is not None else steering_prompt(default_key)
        event = {"type": "response.steer", "previous_response_id": target, "input": steer_text}
        if self.case == "unknown-response":
            event["previous_response_id"] = "resp_00000000000000000000000000000000"
        elif self.case == "missing-target":
            del event["previous_response_id"]
        elif self.case == "empty-input":
            event["input"] = ""
        elif self.case == "empty-array":
            event["input"] = []
        elif self.case in {"array-user", "array-multiple-user", "assistant-message"}:
            messages = ([steering_prompt("ws-multiple-user-first"), steering_prompt("ws-multiple-user-second")]
                        if self.case == "array-multiple-user" else [steer_text])
            event["input"] = [{"role": "assistant" if self.case == "assistant-message" else "user",
                               "content": [{"type": "input_text", "text": text}]} for text in messages]
        elif self.case == "array-image-user":
            from multi_turn_steering.fixtures import steering_media_input

            event["input"] = steering_media_input(self.case)
        elif self.case == "extra-field":
            event["instructions"] = steering_prompt("ws-extra-field")
        elif self.case == "stream-id-field":
            event["stream_id"] = "steering-probe-lane"
        elif self.case == "tool-output-input":
            event["input"] = [{"type": "function_call_output", "call_id": "call_steering_probe_unknown",
                               "output": steering_prompt("ws-tool-output-invalid")}]
        count = self.args.burst_count if self.case == "queue-pressure" else 3 if self.case == "burst" else 1
        for index in range(count):
            current = dict(event)
            if count > 1:
                current["input"] = steering_prompt("ws-burst").format(steer=steer_text, index=index + 1)
            self.send(current, trigger)
            self.steer_submissions.append({"input": current["input"],
                                           "previous_response_id": current.get("previous_response_id"),
                                           "acknowledged": False, "steer_id": None, "events": []})
            self.sent += 1
        self.injected = True

    def track_control(self, event):
        kind, steer = event["type"], event.get("steer", {})
        steer_id = steer.get("id")
        submission = next((item for item in self.steer_submissions if steer_id and item["steer_id"] == steer_id), None)
        correlation = "steer_id"
        if submission is None:
            candidates = [item for item in self.steer_submissions
                          if ("input" not in steer or item["input"] == steer["input"])
                          and ("previous_response_id" not in steer or item["previous_response_id"] == steer["previous_response_id"])]
            unacknowledged = [item for item in candidates if not item["acknowledged"]]
            if unacknowledged:
                submission = unacknowledged[0]
                correlation = "echoed_input" if "input" in steer else "acknowledgement_order"
            elif len(candidates) == 1 and "input" in steer:
                submission = candidates[0]
                correlation = "echoed_input"
        if submission is not None:
            submission["events"].append(kind)
            submission["latest_event"] = kind
            submission["correlation"] = correlation
            if steer_id:
                submission["steer_id"] = steer_id
            if kind in {"response.steer.accepted", "response.steer.failed"}:
                submission["acknowledged"] = True
        if steer_id:
            outcome = self.steer_outcomes.setdefault(steer_id, {"events": [], "acknowledged": False})
            outcome["events"].append(kind)
            outcome["latest_event"] = kind
            if kind in {"response.steer.accepted", "response.steer.failed"}:
                outcome["acknowledged"] = True
        elif submission is None and kind in {"response.steer.accepted", "response.steer.failed"}:
            self.findings.setdefault("uncorrelated_acknowledgements", []).append(event)

    def receive(self):
        event = self.session.record_server_frame()
        kind = event.get("type", "<missing>")
        self.events[kind] += 1
        self.findings["event_counts"] = dict(self.events)
        response = event.get("response", {})
        item = event.get("item", {})
        item_id = event.get("item_id") or item.get("id")
        agent = (event.get("agent") or item.get("agent") or {}).get("agent_name")
        if item_id and agent:
            self.item_agents[item_id] = agent
        if kind.startswith("response.code_interpreter_call.") or (
            kind == "response.output_item.done" and item.get("type") == "code_interpreter_call"
        ):
            state = self.findings["hosted_tools"].setdefault(item_id or "<missing>", [])
            state.append({"event": kind, "ordinal": self.session.recorder.sequence - 1,
                          "agent": agent or self.item_agents.get(item_id), "status": item.get("status")})
        if kind == "response.created":
            self.created.append(response["id"])
            self.awaiting_explicit_response = False
        if kind in STEERING_TERMINALS:
            self.terminals[response["id"]] = {
                "type": kind, "status": response.get("status"),
                "incomplete_details": response.get("incomplete_details"),
                "previous_response_id": response.get("previous_response_id"), "usage": response.get("usage"),
                "output_types": [item.get("type") for item in response.get("output", [])],
            }
            self.calls = [item for item in response.get("output", []) if item.get("type") == "function_call"]
        if kind == "response.output_item.done" and event.get("item", {}).get("type") == "function_call":
            self.calls.append(event["item"])
        if kind.startswith("response.steer.") or kind == "error":
            self.controls.append({"frame_ordinal": self.session.recorder.sequence - 1, "event": event})
        if kind in {"response.steer.accepted", "response.steer.failed", "response.steer.pending"}:
            self.track_control(event)
        self.findings["unacknowledged_submission_count"] = max(0, self.sent - self.acknowledged_count())
        if kind == "error":
            self.generic_error = True
        if kind == "response.steer.pending":
            self.pending_seen = True
        return event

    @staticmethod
    def invalid_tool_output_error(event):
        """Recognize input rejection before retrying a captured tool continuation.

        Providers differ in whether `param` is reported and whether output item
        names use spaces or their wire spelling. Do not retry transport/upstream
        errors or unrelated request validation just because their status is 400.
        """
        error = event.get("error") or {}
        if (event.get("type") != "error" or event.get("status") != 400
                or error.get("type") != "invalid_request_error"):
            return False
        param = str(error.get("param") or "")
        message = str(error.get("message") or "").lower()
        output_names = ("tool output", "function_call_output", "custom_tool_call_output",
                        "shell_call_output", "tool_search_output")
        return (param == "input" or param.startswith(("input.", "input["))
                or ("call" in message and any(name in message for name in output_names)))

    def pending_after_parent_budget(self, event, parent, calls):
        """A submitted steer can still owe output for a completed parent call."""
        response = event.get("response", {})
        return (event.get("type") == "response.incomplete" and self.sent
                and response.get("id") == parent
                and (response.get("incomplete_details") or {}).get("reason") == "max_output_tokens"
                and any(call.get("type") == "function_call" and call.get("call_id")
                        and call.get("status") == "completed" for call in calls))

    def event_agent(self, event):
        item = event.get("item", {})
        return (event.get("agent") or item.get("agent") or {}).get("agent_name", "") or self.item_agents.get(
            event.get("item_id") or item.get("id"), ""
        )

    def advanced_trigger(self, event):
        kind = event.get("type")
        if self.case in STEERING_HOSTED_TOOL_CASES:
            return kind == "response.code_interpreter_call.interpreting"
        return False

    def record_trigger(self, event):
        item = event.get("item", {})
        self.findings["trigger_reached"] = True
        self.findings["steering_trigger"] = {
            "event": event.get("type"), "ordinal": self.session.recorder.sequence - 1,
            "item_id": event.get("item_id") or item.get("id"), "output_index": event.get("output_index"),
            "agent": self.event_agent(event), "response_id": self.created[0] if self.created else None,
            "parent_terminal_observed": bool(self.created and self.created[0] in self.terminals),
            "ordering_only": "A received activity event does not prove server execution was still active when steering arrived.",
        }

    def return_pending_outputs(self):
        self.awaiting_explicit_response = True
        self.create(previous_response_id=self.created[0], input=self.outputs(self.calls), tools=[],
                    instructions=steering_prompt("ws-tool-instructions"))
        self.returned_output = True

    def wait_for_output_release(self):
        """Withhold output for a bounded interval without interrupting a partial frame read."""
        if self.output_release_at is None:
            return
        remaining = self.output_release_at - time.monotonic()
        if remaining > 0:
            ws = self.session.ws
            buffered = bool(getattr(ws, "_receive_buffer", b""))
            tls_pending = getattr(ws.sock, "pending", lambda: 0)()
            if buffered or tls_pending or select.select([ws.sock], [], [], remaining)[0]:
                return
        if len(self.created) > 1:
            self.findings["successor_before_explicit_tool_output"] = True
            self.output_release_at = None
            return
        self.findings["tool_output_withheld_seconds"] = time.monotonic() - self.output_wait_started
        self.output_release_at = None
        self.return_pending_outputs()

    def acknowledged_count(self):
        # accepted followed by failed for the same submission counts only once.
        return sum(item["acknowledged"] for item in self.steer_submissions)

    def bootstrap_reconnect_target(self, target):
        """Admit the fresh socket before testing an ID from a previous connection."""
        self.findings.update(prior_response_id=target, reconnect_bootstrap_completed=False)
        self.send({"type": "response.create", "model": self.args.model, "store": True,
                   "max_output_tokens": self.args.max_output_tokens,
                   "input": steering_prompt("baseline"), "tools": []}, "fresh-connection-bootstrap")
        while True:
            event = self.receive()
            kind = event.get("type")
            if kind == "error" or kind.startswith("response.refusal."):
                raise SteeringPrerequisiteMissing("fresh-connection bootstrap failed/refused before old-target steering")
            if kind in STEERING_TERMINALS:
                if kind != "response.completed" or not self.created:
                    raise SteeringPrerequisiteMissing("fresh-connection bootstrap did not complete before old-target steering")
                self.findings.update(reconnect_bootstrap_completed=True,
                                     reconnect_bootstrap_response_id=event["response"]["id"])
                return

    def observation_ready(self):
        """A settling window records quiet, never proves that later work is impossible."""
        if self.generic_error:
            return True
        if not self.created or any(rid not in self.terminals for rid in self.created):
            return False
        if self.awaiting_explicit_response:
            return False
        if self.output_release_at is not None:
            return False
        if self.acknowledged_count() < self.sent:
            return False
        if self.pending_seen and self.case in {"pending-withheld", "queue-pressure"}:
            return True
        if self.case == "pending-user-steer" and self.pending_seen and self.sent >= 2:
            return True
        if self.case == "pending-steer-then-output" and self.sent >= 2 and not self.returned_output:
            return any(item.get("latest_event") == "response.steer.failed" for item in self.steer_submissions)
        failures = sum(item.get("latest_event") == "response.steer.failed" for item in self.steer_submissions)
        if failures >= self.sent and self.sent:
            return True
        expected_responses = 3 if self.case == "three-turn-steer" else 2
        return len(self.created) >= expected_responses and self.events["response.steer.accepted"] > 0

    def run(self, *, reconnect_target=None, resume_output=None):
        if reconnect_target:
            self.bootstrap_reconnect_target(reconnect_target)
            self.steer(reconnect_target, "fresh-connection-after-bootstrap-completed")
        elif resume_output:
            self.findings["resume_output_sent"] = True
            target, calls = resume_output
            self.create(previous_response_id=target, input=self.outputs(calls), tools=[],
                        instructions=steering_prompt("ws-resume-instructions"))
        else:
            self.create()
        while not self.stop:
            self.wait_for_output_release()
            ready = self.observation_ready()
            if resume_output and self.terminals:
                ready = True
            self.session.ws.sock.settimeout(self.args.quiet_seconds if ready else self.args.timeout)
            try:
                event = self.receive()
            except socket.timeout:
                if ready:
                    self.findings["settled_for_seconds"] = self.args.quiet_seconds
                    return None
                raise
            kind = event.get("type")
            rid = event.get("response", {}).get("id")
            if reconnect_target and kind in {"error", "response.steer.failed"}:
                self.findings["reconnect_steer_rejection"] = event
                return None
            if resume_output:
                if kind == "error":
                    self.findings["resume_output_error"] = event
                    return None
                if kind in STEERING_TERMINALS:
                    self.findings["resume_output_terminal"] = self.terminals[rid]
                continue
            if reconnect_target:
                continue
            if self.case in STEERING_ADVANCED_CASES and not self.injected and (
                kind == "error" or kind.startswith("response.refusal.")
            ):
                raise SteeringPrerequisiteMissing("provider error/refusal before advanced trigger; steering was not exercised")
            if kind in STEERING_TERMINALS and not self.created:
                raise SteeringPrerequisiteMissing("response ended before response.created; inspect the server failure")
            if self.case == "reconnect-target":
                if kind == "error":
                    raise SteeringPrerequisiteMissing("original response failed before the reconnect prerequisite")
                if kind in STEERING_TERMINALS:
                    if kind != "response.completed":
                        raise SteeringPrerequisiteMissing("original response did not complete before reconnect")
                    self.stop = True
                    return rid, []
                continue
            if not self.injected:
                trigger = "response.created"
                if self.case == "steer-delta":
                    trigger = "response.output_text.delta"
                elif self.case == "late-completed":
                    trigger = "response.completed"
                elif self.case in STEERING_TOOL_CASES:
                    trigger = "response.completed" if self.case in STEERING_POST_TERMINAL_TOOL_CASES else "response.output_item.done"
                elif self.case in STEERING_HOSTED_TOOL_CASES:
                    trigger = None
                if kind == trigger or self.advanced_trigger(event):
                    if self.case in STEERING_TOOL_CASES - STEERING_POST_TERMINAL_TOOL_CASES and event.get("item", {}).get("type") != "function_call":
                        continue
                    if self.case in STEERING_TOOL_CASES and not self.calls:
                        raise SteeringPrerequisiteMissing("no function call was observed; steering was not submitted")
                    self.record_trigger(event)
                    self.steer(self.created[0], kind)
                elif kind in STEERING_TERMINALS and (self.case == "steer-delta" or self.case in STEERING_TOOL_CASES | STEERING_ADVANCED_CASES):
                    raise SteeringPrerequisiteMissing("response ended before the requested injection trigger")
            elif self.case == "three-turn-steer" and kind == "response.created" and len(self.created) == 2:
                self.steer(rid, "successor-response.created")
            if kind == "response.steer.pending" and self.case == "disconnect-pending":
                self.stop = True
                return self.created[0], self.calls
            if kind == "response.steer.pending" and self.case in STEERING_POST_TERMINAL_TOOL_CASES:
                if self.sent == 1:
                    self.findings["second_steer_while_output_missing"] = True
                    self.steer(self.created[0], "response.steer.pending", steering_prompt("ws-pending-second-steer"))
                elif self.case == "pending-steer-then-output" and not self.returned_output and self.output_release_at is None:
                    if len(self.steer_submissions) >= 2 and self.steer_submissions[1].get("latest_event") == "response.steer.pending":
                        self.output_wait_started = time.monotonic()
                        self.output_release_at = self.output_wait_started + self.args.quiet_seconds
            if kind == "response.steer.pending" and self.case in {"pending-tool", "pending-wrong-output"}:
                if not self.returned_output:
                    if not self.calls:
                        raise SteeringPrerequisiteMissing("pending arrived without an observed completed function call")
                    outputs = self.outputs(self.calls)
                    if self.case == "pending-wrong-output":
                        outputs[0]["call_id"] = "call_steering_probe_unknown"
                    self.awaiting_explicit_response = True
                    self.create(previous_response_id=self.created[0], input=outputs, tools=[],
                                instructions=steering_prompt("ws-tool-instructions"))
                    self.returned_output = True

    @staticmethod
    def outputs(calls):
        return [{"type": "function_call_output", "call_id": call["call_id"],
                 "output": steering_prompt("function-output")} for call in calls]


class SteeringExtendedProbe(SteeringProbe):
    """Timing, recovery, and request-setting observations without assumed outcomes."""

    def create(self, **overrides):
        if self.case.startswith("settings-"):
            from multi_turn_steering.fixtures import steering_settings_profile

            profile = "explicit" if self.case == "settings-explicit" and overrides else "parent"
            body = {"type": "response.create", "model": self.args.model,
                    "input": steering_prompt("settings-request")}
            body.update(steering_settings_profile(profile, max_output_tokens=self.args.max_output_tokens))
            if self.case == "settings-explicit" and not overrides:
                body.update(input=steering_prompt("function-request"))
            body.update(overrides)
            self.findings.setdefault("settings_requests", []).append({"profile": profile, "request": body})
            self.send(body, "settings-" + profile)
        elif self.case == "pending-output-recovery" and not overrides:
            super().create(tools=[dict(STEERING_TOOL, description=steering_prompt("function-description"))],
                           tool_choice="auto",
                           parallel_tool_calls=False, input=steering_prompt("function-request"))
        else:
            super().create(**overrides)

    def run(self, **_unused):
        self.findings.update(second_boundary_reached=False, invalid_output_sent=False,
                             invalid_output_rejected=False, recovery_output_sent=False,
                             explicit_output_sent=False, successor_final_observed=False,
                             response_created_ordinals=[])
        saved_calls = {}
        needs_tool = self.case in {"pending-output-recovery", "settings-explicit"}
        self.create()
        while True:
            first_accepted = any("response.steer.accepted" in x["events"] for x in self.steer_submissions[:1])
            any_rejected = any(x.get("latest_event") == "response.steer.failed" for x in self.steer_submissions)
            settled = (bool(self.created) and all(r in self.terminals for r in self.created)
                       and self.acknowledged_count() >= self.sent and not self.awaiting_explicit_response)
            # Accepted updates can remain pending; a terminal parent alone is insufficient.
            steer_sends = [entry for entry in self.findings["submissions"] if entry["type"] == "response.steer"]
            accepted_without_later_create = any(
                "response.steer.accepted" in entry["events"] and entry.get("latest_event") != "response.steer.failed"
                and not any(created["ordinal"] >= steer_sends[index]["frame_ordinal_before_send"]
                            for created in self.findings["response_created_ordinals"][1:])
                for index, entry in enumerate(self.steer_submissions)
            )
            ready = settled and not accepted_without_later_create and (len(self.created) > 1 or any_rejected)
            if self.case == "pending-output-recovery" and self.findings["invalid_output_sent"]:
                ready = ready and self.findings["recovery_output_sent"]
            self.session.ws.sock.settimeout(self.args.quiet_seconds if ready else self.args.timeout)
            try:
                event = self.receive()
            except socket.timeout:
                if ready:
                    self.findings["settled_for_seconds"] = self.args.quiet_seconds
                    return
                raise
            kind = event.get("type")
            response = event.get("response", {})
            parent = self.created[0] if self.created else None
            if kind == "response.created":
                self.findings["response_created_ordinals"].append({
                    "response_id": response["id"], "ordinal": self.session.recorder.sequence - 1,
                })
            if self.case.startswith("settings-") and kind in STEERING_TERMINALS | {"response.created"}:
                self.findings.setdefault("settings_responses", []).append({
                    "event": kind, "response_id": response.get("id"),
                    "settings": {key: response.get(key) for key in (
                        "instructions", "tools", "tool_choice", "parallel_tool_calls", "reasoning",
                        "text", "max_output_tokens", "max_tool_calls", "store", "previous_response_id")},
                })
            if kind == "error":
                invalid_tool_input = self.invalid_tool_output_error(event)
                if (self.case == "pending-output-recovery" and self.findings["invalid_output_sent"]
                        and not self.findings["recovery_output_sent"] and invalid_tool_input):
                    self.findings.update(invalid_output_rejected=True, invalid_output_error=event,
                                         recovery_output_sent=True)
                    self.generic_error = False
                    self.awaiting_explicit_response = True
                    self.create(previous_response_id=parent, input=self.outputs(list(saved_calls.values())),
                                tools=[], instructions=steering_prompt("ws-tool-instructions"))
                    continue
                self.findings["extended_request_error"] = event
                self.findings["error_after_steering"] = self.sent > 0
                return
            item = event.get("item", {})
            if kind == "response.output_item.done" and item.get("type") == "function_call":
                saved_calls[item["call_id"]] = item
            if kind in STEERING_TERMINALS and response.get("id") == parent:
                for call in response.get("output", []):
                    if call.get("type") == "function_call":
                        saved_calls[call["call_id"]] = call
            if kind == "response.failed":
                self.findings["extended_terminal_failure"] = event
                return
            if (kind == "response.incomplete" and
                    (response.get("incomplete_details") or {}).get("reason") != "steered"):
                parent_budget_wait = (needs_tool
                    and self.pending_after_parent_budget(event, parent, saved_calls.values()))
                if not parent_budget_wait:
                    self.findings["extended_terminal_failure"] = event
                    return
                # A closed client call still owes its output. Preserve the budget
                # terminal, then await pending before exercising invalid/correct
                # outputs; this does not establish completion of the successor.
                self.findings["parent_budget_terminal"] = {
                    "response_id": parent, "ordinal": self.session.recorder.sequence - 1,
                    "incomplete_details": response.get("incomplete_details"),
                }
            if kind == "response.steer.failed" and all(
                    entry.get("latest_event") == "response.steer.failed" for entry in self.steer_submissions):
                # Every submitted update was explicitly rejected. No successor is owed.
                self.findings["all_steering_rejected"] = True
                return
            first_trigger = (kind == "response.output_item.done" and item.get("type") == "function_call"
                             if needs_tool else kind == "response.created")
            if not self.sent and first_trigger:
                self.record_trigger(event)
                update = steering_prompt("settings-steer") if self.case.startswith("settings-") else None
                self.steer(parent, kind, update)
            elif not self.sent and (kind in STEERING_TERMINALS or kind.startswith("response.refusal.")):
                raise SteeringPrerequisiteMissing("response ended before the required steering trigger")
            if self.case.startswith("old-parent-") and self.sent == 1:
                before = self.case == "old-parent-before-successor"
                boundary = ((kind in STEERING_TERMINALS and response.get("id") == parent and len(self.created) == 1)
                            if before else kind == "response.created" and len(self.created) == 2)
                if boundary:
                    self.findings.update(second_boundary_reached=True, second_target=parent,
                                         second_boundary_event=kind, second_boundary_ordinal=self.session.recorder.sequence - 1,
                                         successor_ids_observed_before_second=list(self.created[1:]),
                                         first_accepted_at_second=first_accepted,
                                         timing_scope="Observed receive order only; server processing may already have advanced.")
                    self.steer(parent, kind + "-old-parent", steering_prompt("old-parent-second-steer"))
            if kind == "response.steer.pending" and needs_tool and not self.returned_output:
                if not saved_calls:
                    raise SteeringPrerequisiteMissing("pending arrived without an actual completed function call")
                outputs = self.outputs(list(saved_calls.values()))
                if self.case == "pending-output-recovery":
                    outputs[0]["call_id"] = "call_steering_probe_unknown"
                    self.findings["invalid_output_sent"] = True
                    self.create(previous_response_id=parent, input=outputs, tools=[],
                                instructions=steering_prompt("ws-tool-instructions"))
                else:
                    self.findings["explicit_output_sent"] = True
                    self.create(previous_response_id=parent, input=outputs)
                self.awaiting_explicit_response = True
                self.returned_output = True
            if kind in STEERING_TERMINALS and response.get("id") != parent:
                if (self.case == "pending-output-recovery" and self.findings["invalid_output_sent"]
                        and not self.findings["recovery_output_sent"]):
                    raise SteeringPrerequisiteMissing("invalid tool continuation reached a terminal response without the required error; recovery was not exercised")
                if kind == "response.completed":
                    self.findings["successor_final_observed"] = True


class SteeringPendingMatrixProbe(SteeringProbe):
    """Observe explicit continuations without conflating a branch with queue consumption."""

    def run(self, **_unused):
        from multi_turn_steering.fixtures import steering_settings_profile

        facts = self.findings
        facts.update(matrix_phase="parent", matrix_prerequisite_reached=False,
                     matrix_outcome_observed=False, matrix_rejection=False,
                     lane_responses={}, settings_requests=[], settings_responses=[])
        calls, response_lanes = {}, {}
        parent = None
        phase = "parent"
        phase_response = None
        is_lane = self.case == "lane-parent-mismatch"
        partial = self.case == "pending-partial-outputs"
        defaults = self.case == "settings-explicit-defaults"
        store = self.case != "pending-output-recovery-store-false"
        status_tool = dict(STEERING_TOOL, description=steering_prompt("function-description"))
        initial = {"type": "response.create", "model": self.args.model, "store": store,
                   "max_output_tokens": self.args.max_output_tokens,
                   "input": steering_prompt("function-request"), "tools": [status_tool],
                   "tool_choice": "auto",
                   "parallel_tool_calls": False}
        if defaults:
            initial.update(steering_settings_profile("parent", max_output_tokens=self.args.max_output_tokens))
        if partial:
            initial.update(input=steering_prompt("partial-tools-request"),
                           parallel_tool_calls=True,
                           tools=[status_tool, dict(STEERING_TOOL, name="get_project_deadline",
                                       description=steering_prompt("function-deadline-description"))])
        if is_lane:
            initial["stream_id"] = "steering-lane-a"
        facts["settings_requests"].append({"phase": "parent", "request": initial})
        self.send(initial, "initial")

        def actual_outputs():
            return [{"type": "function_call_output", "call_id": call["call_id"],
                     "output": steering_prompt("function-deadline-output" if call.get("name") ==
                                               "get_project_deadline" else "function-output")}
                    for call in calls.values()]

        def submit(next_phase, outputs, lane=None):
            nonlocal phase, phase_response
            phase, phase_response = next_phase, None
            body = {"type": "response.create", "model": self.args.model,
                    "previous_response_id": parent, "input": outputs,
                    "max_output_tokens": self.args.max_output_tokens}
            if not defaults:
                body.update(store=store, tools=[], instructions=steering_prompt("ws-tool-instructions"))
            if lane:
                body["stream_id"] = lane
            facts["matrix_phase"] = phase
            facts["settings_requests"].append({"phase": phase, "request": body})
            self.awaiting_explicit_response = True
            self.send(body, phase)

        while True:
            self.session.ws.sock.settimeout(self.args.timeout)
            event = self.receive()
            kind, response = event.get("type"), event.get("response", {})
            rid = response.get("id") or event.get("response_id")
            explicit_lane = event.get("stream_id") or response.get("stream_id")
            if kind == "response.created":
                if parent is None:
                    parent = response["id"]
                elif phase not in {"parent", "pending"}:
                    phase_response = response["id"]
                response_lanes[response["id"]] = explicit_lane
            lane = explicit_lane or response_lanes.get(rid)
            if is_lane:
                # Missing lane metadata stays missing; do not infer proof from our request.
                facts.setdefault("lane_events", []).append({"event": kind, "response_id": rid,
                    "stream_id": lane, "phase": phase, "ordinal": self.session.recorder.sequence - 1})
            if kind in STEERING_TERMINALS | {"response.created"}:
                facts["settings_responses"].append({"event": kind, "response_id": rid,
                    "stream_id": lane, "phase": phase, "settings": {key: response.get(key) for key in (
                        "instructions", "tools", "tool_choice", "parallel_tool_calls", "reasoning", "text",
                        "max_output_tokens", "max_tool_calls", "store", "previous_response_id")}})
            if kind in STEERING_TERMINALS:
                facts["lane_responses"].setdefault(lane or "<unreported>", []).append({
                    "response_id": rid, "event": kind, "phase": phase,
                    "previous_response_id": response.get("previous_response_id"),
                    "output_text": "".join(content.get("text", "") for item in response.get("output", [])
                                           for content in item.get("content", []) if isinstance(content, dict))})
            item = event.get("item", {})
            if phase == "parent" and kind == "response.output_item.done" and item.get("type") == "function_call":
                calls[item["call_id"]] = item
            if rid == parent and kind in STEERING_TERMINALS:
                for call in response.get("output", []):
                    if call.get("type") == "function_call" and call.get("call_id"):
                        calls[call["call_id"]] = call
            if kind == "error":
                error = event.get("error") or {}
                invalid_input = self.invalid_tool_output_error(event)
                facts.setdefault("matrix_errors", []).append({"phase": phase, "event": event})
                if phase == "invalid-output" and invalid_input:
                    facts.update(invalid_output_rejected=True, matrix_prerequisite_reached=True)
                    self.generic_error = False
                    submit("recovery", actual_outputs())
                    continue
                client_rejection = (event.get("status") in {400, 404, 409, 422}
                                    or error.get("code") == "previous_response_not_found")
                if phase == "wrong-lane" and client_rejection:
                    facts["wrong_lane_outcome"] = "request_rejected"
                    self.generic_error = False
                    submit("original-lane", actual_outputs(), "steering-lane-a")
                    continue
                if phase in {"recovery", "original-lane", "defaults"} and client_rejection:
                    facts.update(matrix_outcome_observed=True, matrix_rejection=True)
                else:
                    facts["matrix_request_failure"] = True
                return
            if kind == "response.steer.failed":
                facts.update(matrix_steer_rejected=True, matrix_rejection=True)
                return
            if (rid == parent and kind in STEERING_TERMINALS and kind != "response.completed"
                    and (response.get("incomplete_details") or {}).get("reason") != "steered"):
                parent_budget_wait = (
                    self.case in {"settings-explicit-defaults", "pending-output-recovery-store-false"}
                    and phase == "pending"
                    and self.pending_after_parent_budget(event, parent,
                        (call for call in calls.values() if call.get("name") == "get_project_status")))
                if not parent_budget_wait:
                    facts["matrix_terminal_failure"] = event
                    return
                # Preserve the incomplete parent as evidence. Only pending and
                # the actual continuation/recovery can establish this outcome.
                facts["parent_budget_terminal"] = {
                    "response_id": parent, "ordinal": self.session.recorder.sequence - 1,
                    "incomplete_details": response.get("incomplete_details"),
                }
            if not self.sent:
                trigger = (kind == "response.completed" and rid == parent if partial else
                           kind == "response.output_item.done" and item.get("type") == "function_call")
                if trigger:
                    if partial and (len(calls) < 2 or not {"get_project_status", "get_project_deadline"}.issubset(
                                    {call.get("name") for call in calls.values()})):
                        raise SteeringPrerequisiteMissing("parent did not complete both required function calls")
                    self.record_trigger(event)
                    self.steer(parent, kind, steering_prompt("settings-steer") if defaults else None)
                    phase = "pending"
                    facts["matrix_phase"] = phase
                elif kind in STEERING_TERMINALS or kind.startswith("response.refusal."):
                    raise SteeringPrerequisiteMissing("parent ended before the required function-call trigger")
            if kind == "response.steer.pending" and phase == "pending":
                if not calls:
                    raise SteeringPrerequisiteMissing("pending did not include an actual completed call")
                facts["pending_observed"] = True
                outputs = actual_outputs()
                if defaults:
                    facts["matrix_prerequisite_reached"] = True
                    submit("defaults", outputs)
                elif is_lane:
                    facts["matrix_prerequisite_reached"] = True
                    submit("wrong-lane", outputs, "steering-lane-b")
                else:
                    if partial:
                        outputs = outputs[:1]
                    else:
                        outputs[0]["call_id"] = "call_steering_probe_unknown"
                    submit("invalid-output", outputs)
            if kind in STEERING_TERMINALS and rid != parent:
                if rid != phase_response:
                    # Preserve other lanes' frames without completing the active request.
                    continue
                if phase == "wrong-lane":
                    facts["wrong_lane_outcome"] = kind
                    submit("original-lane", actual_outputs(), "steering-lane-a")
                elif phase == "invalid-output":
                    raise SteeringPrerequisiteMissing("partial/invalid output was not rejected; recovery prerequisite absent")
                elif phase in {"recovery", "original-lane", "defaults"}:
                    if kind == "response.completed":
                        facts["matrix_outcome_observed"] = True
                    else:
                        facts["matrix_terminal_failure"] = event
                    return


class SteeringMixedToolsProbe(SteeringProbe):
    """Reuse original mixed-tool tasks/fixtures, adding steering at observed boundaries."""

    CLIENT_CALLS = {"function_call", "custom_tool_call"}

    def __init__(self, session, args, case, findings):
        super().__init__(session, args, case, findings)
        self.fixture_case = case
        self.queued_outputs = []
        self.calls_seen = set()
        self.outputs_returned = set()
        self.continuations = 0
        self.first_ack_ordinal = None
        findings.update(steering_triggers=[], client_output_submissions=[])

    def create(self, **overrides):
        body = {"type": "response.create", "model": self.args.model,
                "max_output_tokens": self.args.max_output_tokens}
        body.update(steering_request_settings(self.fixture_case))
        body.update(overrides)
        self.send(body, "initial" if not self.created else "client-output-continuation")

    def activity(self, event):
        kind, item = event.get("type"), event.get("item", {})
        if self.sent == 0:
            call_types = {"function_call", "custom_tool_call"}
            return kind == "response.output_item.done" and item.get("type") in call_types and bool(item.get("call_id"))
        return kind in {"response.output_text.delta", "response.code_interpreter_call.interpreting",
                        "response.code_interpreter_call.completed"} or (
            kind in {"response.output_item.added", "response.output_item.done"}
            and item.get("type") in self.CLIENT_CALLS | {"message", "reasoning", "code_interpreter_call", "web_search_call"}
        )

    def is_client_call(self, item):
        return item.get("type") in self.CLIENT_CALLS

    def queue_calls(self, calls):
        fresh = [call for call in calls if self.is_client_call(call) and call.get("call_id") not in self.calls_seen]
        if not fresh:
            return
        outputs = steering_tool_outputs(self.fixture_case, fresh)
        self.calls_seen.update(call["call_id"] for call in fresh)
        self.queued_outputs.extend(outputs)
        if len(self.queued_outputs) > 64:
            raise RuntimeError("client output queue exceeded its 64-entry recording budget")

    def flush_outputs(self):
        if (not self.queued_outputs or not self.created
                or self.sent < 2 or self.acknowledged_count() < 2):
            return
        target = self.created[-1]
        if target in self.terminals:
            self.submit_outputs(target)

    def submit_outputs(self, target):
        if self.continuations >= 10:
            raise SteeringPrerequisiteMissing("client-tool continuation budget exhausted")
        outputs, self.queued_outputs = self.queued_outputs, []
        self.continuations += 1
        self.findings["client_output_submissions"].append({"type": "response.create", "previous_response_id": target,
                                                           "call_ids": [item.get("call_id") for item in outputs]})
        self.awaiting_explicit_response = True
        self.create(previous_response_id=target, input=outputs)
        # Explicit continuation acceptance is observed through its response lifecycle.
        self.outputs_returned.update(item.get("call_id") for item in outputs)

    def complete_enough_to_observe(self):
        if not self.created or any(rid not in self.terminals for rid in self.created):
            return False
        if self.queued_outputs or self.awaiting_explicit_response:
            return False
        if self.acknowledged_count() < self.sent:
            return False
        for submission in self.steer_submissions:
            if "response.steer.accepted" in submission["events"] and submission.get("latest_event") != "response.steer.failed":
                target = submission["previous_response_id"]
                if target in self.created and len(self.created) <= self.created.index(target) + 1:
                    return False
        return True

    def run(self, **_unused):
        self.create()
        while True:
            ready = self.complete_enough_to_observe()
            self.session.ws.sock.settimeout(self.args.quiet_seconds if ready else self.args.timeout)
            try:
                event = self.receive()
            except socket.timeout:
                if ready:
                    self.findings["settled_for_seconds"] = self.args.quiet_seconds
                    self.findings["two_steering_updates_sent"] = self.sent >= 2
                    if self.sent < 2:
                        raise SteeringPrerequisiteMissing("scenario ended before two actual activity boundaries could be steered")
                    return None
                raise
            kind = event.get("type")
            ordinal = self.session.recorder.sequence - 1
            if kind == "error":
                raise SteeringPrerequisiteMissing("provider error during mirrored scenario; unresolved steering/tool outcomes remain in raw capture")
            if kind == "response.output_item.done" and event.get("item", {}).get("type") in self.CLIENT_CALLS:
                self.queue_calls([event["item"]])
            if kind in STEERING_TERMINALS:
                self.queue_calls([item for item in event.get("response", {}).get("output", [])
                                  if item.get("type") in self.CLIENT_CALLS])
            if self.sent and self.acknowledged_count() and self.first_ack_ordinal is None:
                self.first_ack_ordinal = ordinal
            target = self.created[-1] if self.created else None
            held_call_boundary = bool(self.queued_outputs) and self.sent == 1 and self.first_ack_ordinal is not None and (
                kind in {"response.steer.pending", "response.steer.accepted", "response.steer.failed"}
                or (target in self.terminals and kind in STEERING_TERMINALS)
            )
            active_boundary = target and target not in self.terminals and self.activity(event)
            if target and (active_boundary or held_call_boundary):
                if self.sent == 0 or (self.sent == 1 and self.first_ack_ordinal is not None and
                                     (ordinal > self.first_ack_ordinal or held_call_boundary)):
                    self.record_trigger(event)
                    self.findings["steering_trigger"].update(
                        response_id=target, parent_terminal_observed=target in self.terminals,
                        client_outputs_withheld=bool(self.queued_outputs),
                        pending_client_output_ack_boundary=held_call_boundary,
                    )
                    self.findings["steering_triggers"].append(dict(self.findings["steering_trigger"]))
                    self.steer(target, kind, steering_prompt(f"{self.fixture_case}-update-{self.sent + 1}"))
            if not self.injected and (kind in STEERING_TERMINALS or kind == "error" or kind.startswith("response.refusal.")):
                raise SteeringPrerequisiteMissing("original mixed-tool scenario ended/refused before an actual client-call trigger")
            self.flush_outputs()


class SteeringClientToolsProbe(SteeringMixedToolsProbe):
    """Steer each completed client-call batch before returning any of its outputs."""

    CLIENT_CALLS = {"function_call", "custom_tool_call", "shell_call", "tool_search_call"}

    def run(self, **_unused):
        self.findings.update(client_call_types=[], pending_output_types=[], pending_obligations=[])
        expected_required = {}
        self.create()
        while True:
            ready = self.complete_enough_to_observe()
            self.session.ws.sock.settimeout(self.args.quiet_seconds if ready else self.args.timeout)
            try:
                event = self.receive()
            except socket.timeout:
                if ready:
                    self.findings["settled_for_seconds"] = self.args.quiet_seconds
                    return None
                raise
            kind = event.get("type")
            if kind == "error":
                raise SteeringPrerequisiteMissing("client-tool workflow rejected; inspect captured error")
            if kind == "response.steer.failed":
                # Rejection is evidence, not a reason to fabricate a continuation.
                return None
            if kind in STEERING_TERMINALS:
                response = event["response"]
                calls = [item for item in response.get("output", []) if self.is_client_call(item)]
                self.findings["client_call_types"] = sorted(set(self.findings["client_call_types"]) |
                                                           {item["type"] for item in calls})
                if kind != "response.completed":
                    raise SteeringPrerequisiteMissing("client-tool response failed or ended incomplete")
                self.queue_calls(calls)
                if self.queued_outputs:
                    if len(self.queued_outputs) != len(calls):
                        raise SteeringPrerequisiteMissing("not every client call has a fixture output")
                    expected_required = {
                        output["call_id"]: {"type": output["type"], "call_id": output["call_id"]}
                        for output in self.queued_outputs
                    }
                    for call in calls:
                        stub = expected_required[call["call_id"]]
                        if call["type"] == "function_call":
                            stub["name"] = call["name"]
                        if call["type"] == "tool_search_call":
                            stub["execution"] = "client"
                    self.record_trigger(event)
                    self.findings["steering_trigger"].update(
                        response_id=response["id"], parent_terminal_observed=True, client_outputs_withheld=True)
                    self.findings["steering_triggers"].append(dict(self.findings["steering_trigger"]))
                    self.steer(response["id"], kind,
                               steering_prompt("client-tools-steer-update").format(index=self.sent + 1))
            if kind == "response.steer.pending":
                target = event.get("steer", {}).get("previous_response_id")
                if not self.created or target != self.created[-1] or not self.queued_outputs:
                    raise SteeringPrerequisiteMissing("pending event does not identify the withheld client batch")
                required = event.get("required_input", [])
                observed = {item.get("call_id"): item for item in required}
                self.findings["pending_obligations"].append({
                    "previous_response_id": target, "required_input": required,
                    "matches_client_outputs": observed == expected_required and len(required) == len(expected_required),
                })
                self.findings["pending_output_types"] = sorted(set(self.findings["pending_output_types"]) |
                                                              {str(item.get("type")) for item in required})
                self.submit_outputs(target)


def steering_arguments():
    parser = argparse.ArgumentParser(
        description="OpenAI or gateway mid-turn steering observations using the shared duplex recorder.", formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Environment (CLI flags override defaults):
  STEERING_RECORD_SET     openai (default) or gateway; shell also accepts all
  OPENAI_API_KEY          Required only for live OpenAI recording; never printed
  GATEWAY_API_KEY         Optional gateway bearer token; never printed
  GATEWAY_URL             Gateway base URL (default http://localhost:9000)
  GATEWAY_MODEL           Served gateway model; MODEL fallback, required for gateway
  OPENAI_MODEL            Default gpt-6-sol
  STEERING_SUITE          smoke, edges, all, non-vision, vision, client-tools, or a category suite
  STEERING_CASE           One exact case name; overrides suite
  STEERING_TIMEOUT        Wall seconds per case, including reconnect (default 300)
  STEERING_QUIET_SECONDS  Settling window after candidate completion (default 3)
  STEERING_REPEAT         Fresh sessions per case (default 1, maximum 100)
  STEERING_BURST_COUNT    Queue-pressure bound (default 32, maximum 128)
  STEERING_OUTPUT_DIR     Default: multi_turn_steering/ (generated output only)
  MAX_OUTPUT_TOKENS      Per-response limit (default 8192)

Use the shell wrapper for OpenAI or gateway WebSocket observations. Steering is WebSocket-only;
this driver reuses RecordedSession/SessionRecorder and the shared tool fixtures.
--dry-run and --list-cases do not contact an API or write files.
Live runs incur usage; automatic continuations have separate token budgets.
Stable YAML filenames are replaced on rerun. The single-agent mixed-tool workflow
gets one YAML. Edge probes use three category YAMLs: input/lifecycle, tools/settings,
and connections. Image steering uses its own vision-steering YAML.
Turns, reconnect sessions, and repeats stay in their category.
--suite all writes six files; --suite non-vision writes the five text/tool files;
--suite client-tools writes only the client-tools-steer file.
--suite vision writes only the image file. --suite edges writes the three non-vision edge files.
--case edge-cases is an alias for the three edge categories. Individual --case names
remain available for debugging and replace their category file with that case only.
An explicit --output is allowed only when selecting a single category.
Gateway filenames use gateway instead of openai-reference. Its unsupported-model
case is skipped: OpenAI model availability restrictions are not a gateway contract.
No findings JSON is written.
Earlier timestamped runs are untouched. This driver uses the existing recorder;
multi_turn_steering/ holds the shared prompt catalog and generated captures.
Failed/rejected input is evidence, not an assertion of expected provider behavior.
Cleanup has a separate maximum five-second budget. A quiet window does not prove
that a later successor could never be produced. Inspect raw frames and history.
Advanced scenarios include hosted code execution and steering after a completed
client tool call while its output is still withheld. Multi-agent steering is
unsupported and disabled; use the ordinary multi-agent recorder for other features.
Deferred: MCP approvals, conversation persistence, and slow consumers.
""")
    parser.add_argument("--suite", choices=sorted(STEERING_SUITES), default=os.getenv("STEERING_SUITE", "smoke"))
    parser.add_argument("--case", choices=[*STEERING_CASES, *sorted(STEERING_GROUPS)], default=os.getenv("STEERING_CASE"))
    parser.add_argument("--record-set", choices=["openai", "gateway"], default=os.getenv("STEERING_RECORD_SET", "openai"))
    parser.add_argument("--gateway-url", default=os.getenv("GATEWAY_URL", "http://localhost:9000"))
    parser.add_argument("--model", help="Provider model; defaults to OPENAI_MODEL or GATEWAY_MODEL/MODEL")
    parser.add_argument("--unsupported-model", default="gpt-5.6")
    parser.add_argument("--timeout", type=float, default=os.getenv("STEERING_TIMEOUT", "300"))
    parser.add_argument("--quiet-seconds", type=float, default=os.getenv("STEERING_QUIET_SECONDS", "3"))
    parser.add_argument("--repeat", type=int, default=os.getenv("STEERING_REPEAT", "1"))
    parser.add_argument("--burst-count", type=int, default=os.getenv("STEERING_BURST_COUNT", "32"))
    parser.add_argument("--max-output-tokens", type=int, default=os.getenv("MAX_OUTPUT_TOKENS", "8192"))
    default_output = Path(__file__).resolve().parent / "multi_turn_steering"
    parser.add_argument("--output-dir", type=Path, default=Path(os.getenv("STEERING_OUTPUT_DIR", str(default_output))))
    parser.add_argument("--output", type=Path, help="Exact YAML path; selection must contain only one category")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--list-cases", action="store_true")
    args = parser.parse_args()
    if args.record_set not in {"openai", "gateway"}:
        parser.error("STEERING_RECORD_SET must be openai or gateway; use the shell wrapper for all")
    if not args.model:
        args.model = (os.getenv("OPENAI_MODEL", "gpt-6-sol") if args.record_set == "openai"
                      else os.getenv("GATEWAY_MODEL") or os.getenv("MODEL"))
    if not args.model and not args.list_cases:
        parser.error("GATEWAY_MODEL (or MODEL / --model) is required for gateway recordings")
    args.model = args.model or "<served-model>"
    args.endpoint = STEERING_URL
    if args.record_set == "gateway":
        from urllib.parse import urlparse

        url = urlparse(args.gateway_url)
        if (url.scheme not in {"http", "https", "ws", "wss"} or not url.hostname
                or url.username or url.password or url.query or url.fragment):
            parser.error("gateway URL must be an HTTP/WebSocket base URL without credentials, query, or fragment")
        args.endpoint = RecordedSession.responses_url(args.gateway_url)
    if args.suite not in STEERING_SUITES or (args.case and args.case not in {*STEERING_CASES, *STEERING_GROUPS}):
        parser.error("invalid STEERING_SUITE or STEERING_CASE")
    if args.output and len(steering_selection(args)) != 1:
        parser.error("--output requires one category; umbrella edge-cases/suite-all selections cannot share one file")
    if not all(math.isfinite(n) and n > 0 for n in (args.timeout, args.quiet_seconds)):
        parser.error("timeouts must be positive finite numbers")
    if args.quiet_seconds >= args.timeout:
        parser.error("quiet window must be shorter than scenario timeout")
    if not 1 <= args.repeat <= 100 or not 1 <= args.burst_count <= 128 or args.max_output_tokens < 16:
        parser.error("repeat must be 1..100, burst-count 1..128, max-output-tokens at least 16")
    return parser, args


def steering_classify_observation(result, case):
    """Keep capture completion distinct from reaching a scenario's prerequisites."""
    result["status"] = "observed"
    connections = result["connections"]
    if all(not item["created"] for item in connections):
        result["status"] = "unavailable" if case != "reconnect-target" else "observed"
    if case in STEERING_TOOL_CASES and not any(item["event_counts"].get("response.steer.pending", 0) for item in connections):
        result.update(status="inconclusive", detail="required pending-steering state was not observed; inspect controls")
    if case == "three-turn-steer" and len(connections[0]["created"]) < 3:
        result.update(status="inconclusive", detail="the task and two subsequent user inputs did not reach a third response")
    if any(item.get("unacknowledged_submission_count", 0) for item in connections):
        result.update(status="inconclusive", detail="steering submissions remain unacknowledged; inspect raw controls")
    successors = [terminal for item in connections for rid, terminal in item["terminals"].items()
                  if item["created"] and rid != item["created"][0]]
    unfinished = [item for item in successors if item["type"] != "response.completed"
                  and (item.get("incomplete_details") or {}).get("reason") != "steered"]
    if unfinished:
        result.update(status="inconclusive", detail="successor failed or ended incomplete; inspect terminal evidence")
    if case in STEERING_ADVANCED_CASES:
        reached = any(item.get("trigger_reached") for item in connections)
        rejected = any(entry.get("latest_event") == "response.steer.failed"
                       for item in connections for entry in item.get("steering_submissions", []))
        if not reached:
            result.update(status="inconclusive", detail="advanced activity trigger was not observed")
        elif rejected:
            result.update(status="observed_rejection", detail="steering was explicitly rejected after the observed trigger; support is not established")
    if case == "client-tools-steer" and result["status"] == "observed":
        facts = connections[0]
        expected = {"function_call_output", "custom_tool_call_output", "shell_call_output", "tool_search_output"}
        if not expected.issubset(facts.get("pending_output_types", [])):
            result.update(status="inconclusive", detail="not all four client output types reached pending steering")
        elif not all(item["matches_client_outputs"] for item in facts.get("pending_obligations", [])):
            result.update(status="observed_mismatch", detail="pending obligations differ from the actual withheld client calls")
    if case == "reconnect-target" and any(
        item.get("reconnect_bootstrap_completed") and item.get("reconnect_steer_rejection")
        for item in connections
    ):
        result.update(status="observed_rejection", detail="old-target steering was rejected after the fresh connection completed its bootstrap response; inspect the raw error")
    if case in STEERING_EXTENDED_CASES:
        facts = connections[0]
        submissions = facts.get("steering_submissions", [])
        rejected = any(entry.get("latest_event") == "response.steer.failed" for entry in submissions)
        first_rejected = len(submissions) == 1 and rejected
        required = (["second_boundary_reached", "successor_final_observed"] if case.startswith("old-parent-")
                    else ["invalid_output_rejected", "recovery_output_sent", "successor_final_observed"]
                    if case == "pending-output-recovery"
                    else ["explicit_output_sent", "successor_final_observed"] if case == "settings-explicit"
                    else ["successor_final_observed"])
        missing = [name for name in required if not facts.get(name)]
        if missing:
            result.update(status="inconclusive", detail="scenario prerequisites not reached: " + ", ".join(missing))
        if facts.get("extended_request_error") or facts.get("extended_terminal_failure"):
            result.update(status="inconclusive", detail="provider request/response failed; inspect the captured error and reached prerequisites")
        elif (rejected and (first_rejected or not missing) and not unfinished
              and not facts.get("unacknowledged_submission_count", 0)):
            result.update(status="observed_rejection", detail="steering was explicitly rejected; capture records which recovery/timing/settings prerequisites were reached")


    if case in STEERING_PENDING_MATRIX_CASES:
        facts = connections[0]
        if (facts.get("matrix_steer_rejected") and facts.get("trigger_reached")
                and not facts.get("unacknowledged_submission_count", 0)):
            result.update(status="observed_rejection", detail="steering rejected at the actual function boundary; continuation prerequisite was not reached")
        elif not facts.get("matrix_prerequisite_reached") or not facts.get("matrix_outcome_observed"):
            result.update(status="inconclusive", detail="required pending/validation/continuation outcome was not observed; inspect matrix phases")
        elif facts.get("matrix_request_failure") or facts.get("matrix_terminal_failure"):
            result.update(status="inconclusive", detail="provider failed during the continuation probe")
        elif facts.get("unacknowledged_submission_count", 0):
            result.update(status="inconclusive", detail="steering acknowledgement was not observed")
        else:
            result.update(status="observed_rejection" if facts.get("matrix_rejection") else "observed",
                          detail="explicit continuation outcome captured after the required pending/validation boundary")

    if case == "reconnect-target" and not any(
        item.get("reconnect_bootstrap_completed") and item.get("reconnect_steer_rejection")
        for item in connections
    ):
        result.update(status="inconclusive", detail="fresh-connection bootstrap and old-target steering rejection were not both observed")
    if case == "disconnect-pending":
        resumed = connections[1] if len(connections) > 1 else {}
        terminal = resumed.get("resume_output_terminal") or {}
        if not resumed.get("resume_output_sent") or terminal.get("type") != "response.completed":
            result.update(status="inconclusive", detail="fresh-connection tool-output continuation did not complete; connection-local steering isolation remains unproven")
    if case != "reconnect-target" and not any(item.get("steering_submissions") for item in connections):
        result.update(status="inconclusive", detail="no steering input was submitted after the required activity trigger")


def steering_selection(args):
    categories = STEERING_CATEGORIES
    if args.case in STEERING_CASES:
        category = next(name for name, cases in categories.items() if args.case in cases)
        return {category: [args.case]}
    if args.case in categories:
        return {args.case: list(categories[args.case])}
    selection = args.case.removeprefix("suite-") if args.case in STEERING_GROUPS else args.suite
    if selection in {"edges", "edge-cases"}:
        return {name: list(cases) for name, cases in categories.items() if name.startswith("edge-")}
    if selection == "non-vision":
        return {name: list(cases) for name, cases in categories.items() if name != "vision-steering"}
    if selection == "vision":
        return {"vision-steering": list(categories["vision-steering"])}
    if selection == "client-tools":
        return {"client-tools-steer": list(categories["client-tools-steer"])}
    if selection == "all":
        return {name: list(cases) for name, cases in categories.items()}
    if selection == "smoke":
        return {"mixed-tools-steer": list(STEERING_SMOKE)}
    category = "edge-" + selection
    return {category: list(categories[category])}


def steering_main():
    parser, args = steering_arguments()
    selected_categories = steering_selection(args)
    skipped_cases = {}
    if args.record_set == "gateway":
        for category, cases in selected_categories.items():
            if "unsupported-model" in cases:
                cases.remove("unsupported-model")
                skipped_cases["unsupported-model"] = "OpenAI model availability restrictions are not modeled by the gateway; use the served GATEWAY_MODEL"
        selected_categories = {name: cases for name, cases in selected_categories.items() if cases}
    selected = [case for cases in selected_categories.values() for case in cases]
    case_categories = {case: category for category, cases in selected_categories.items() for case in cases}
    model_slug = "".join(char if char.isalnum() or char in "._-" else "-" for char in args.model)
    provider_label = "openai-reference" if args.record_set == "openai" else "gateway"
    outputs = {category: args.output or args.output_dir /
               f"multi-turn-steering-{provider_label}-{category}-{model_slug}-websocket.yaml"
               for category in selected_categories}
    for case, reason in skipped_cases.items():
        print(f"{case}: skipped ({reason})")
    if args.list_cases or args.dry_run:
        for case in STEERING_CASES if args.list_cases else selected:
            category = next(name for name, cases in STEERING_CATEGORIES.items() if case in cases)
            print(f"{case} [{category}]: {STEERING_CASES[case]}")
        if args.dry_run:
            print(json.dumps({"record_set": args.record_set, "endpoint": args.endpoint, "model": args.model, "repeat": args.repeat,
                              "skipped_cases": skipped_cases,
                              "timeout_seconds": args.timeout, "max_output_tokens_per_response": args.max_output_tokens,
                              "output_parent": str(args.output_dir), "cases": selected,
                              "categories": selected_categories,
                              "output_files": {name: str(path) for name, path in outputs.items()}}, indent=2))
            print("Dry run complete; no files written and no API requests sent.")
        return 0
    if not selected:
        print("No eligible scenarios selected; no files written and no API requests sent.")
        return 0
    key = os.getenv("OPENAI_API_KEY" if args.record_set == "openai" else "GATEWAY_API_KEY", "")
    if args.record_set == "openai" and not key:
        parser.error("export OPENAI_API_KEY before a live OpenAI recording")
    if not hasattr(signal, "setitimer"):
        parser.error("live probes require POSIX interval timers for bounded capture")
    try:
        import record_cassette as recorder
    except ImportError:
        parser.error("recorder dependencies are missing; see wrapper --help for the uv command")

    class RedactedRecorder(SessionRecorder):
        def _write(self, value, indent):
            super()._write(steering_sanitize(value, key), indent)

    class BoundedSession(RecordedSession):
        def __exit__(self, error_type, error, traceback):
            # Bound late-frame draining during the shared close handshake too.
            signal.setitimer(signal.ITIMER_REAL, 5)
            try:
                return super().__exit__(error_type, error, traceback)
            finally:
                signal.setitimer(signal.ITIMER_REAL, 0)

    headers = {"Authorization": f"Bearer {key}"} if key else {}
    args.output_dir.mkdir(parents=True, exist_ok=True)
    results = []
    captures = {}
    interrupted = False
    signal.signal(signal.SIGALRM, steering_expire)

    def terminate(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    for repeat in range(1, args.repeat + 1):
        for case in selected:
            case_model = args.unsupported_model if case == "unsupported-model" else args.model
            category = case_categories[case]
            output = outputs[category]
            output.parent.mkdir(parents=True, exist_ok=True)
            result = {"case": case, "attempt": repeat, "description": STEERING_CASES[case], "cassette": output.name,
                      "status": "running", "connections": []}
            results.append(result)
            if output not in captures:
                if args.case in STEERING_CASES and case not in STEERING_SMOKE:
                    print(f"Selected case replaces category {category} at {output}; other category cases are omitted.", flush=True)
                captures[output] = RedactedRecorder(output)
            capture = captures[output]
            request_headers = dict(headers)
            started = time.monotonic()
            try:
                signal.setitimer(signal.ITIMER_REAL, args.timeout)
                continuation = None
                for connection in range(2 if case in {"reconnect-target", "disconnect-pending"} else 1):
                    remaining = args.timeout - (time.monotonic() - started)
                    if remaining <= 0:
                        raise SteeringBudgetExpired("scenario budget exhausted before next connection")
                    signal.setitimer(signal.ITIMER_REAL, remaining)
                    findings = {"connection_index": connection}
                    result["connections"].append(findings)
                    captured_headers = {"authorization": "Bearer ***"} if key else {}
                    handshake = {"request": {"method": "GET", "url": args.endpoint, "headers": captured_headers},
                                 "probe": {"case": case, "category": category, "attempt": repeat, "connection_index": connection,
                                           "timeout_seconds": args.timeout, "model": case_model,
                                           "provider": args.record_set, "source": STEERING_GUIDE}}
                    with BoundedSession(recorder.WebSocketClient(args.endpoint, request_headers), output, handshake, args.timeout,
                                         session_recorder=capture) as session:
                        session.probe_result = findings
                        driver_type = (SteeringPendingMatrixProbe if case in STEERING_PENDING_MATRIX_CASES
                                       else SteeringExtendedProbe if case in STEERING_EXTENDED_CASES
                                       else SteeringClientToolsProbe if case == "client-tools-steer"
                                       else SteeringMixedToolsProbe if case in STEERING_MIRRORED_CASES else SteeringProbe)
                        driver = driver_type(session, args, case, findings)
                        kwargs = {}
                        if connection:
                            if not continuation:
                                raise SteeringPrerequisiteMissing("first connection did not reach the reconnect trigger")
                            kwargs = {"reconnect_target": continuation[0]} if case == "reconnect-target" else {"resume_output": continuation}
                        continuation = driver.run(**kwargs)
                        findings["unacknowledged_submission_count"] = max(0, driver.sent - driver.acknowledged_count())
                steering_classify_observation(result, case)
            except SteeringPrerequisiteMissing as error:
                result.update(status="inconclusive", detail=str(error))
            except TimeoutError as error:
                result.update(status="timeout", detail=str(error))
            except KeyboardInterrupt:
                result.update(status="interrupted")
                interrupted = True
            except (OSError, RuntimeError, ValueError, EOFError) as error:
                result.update(status="unavailable" if not capture.sequence else "transport_or_driver_failure", detail=str(error))
            except Exception as error:
                result.update(status="transport_or_driver_failure", detail=f"{type(error).__name__}: {error}")
            finally:
                signal.setitimer(signal.ITIMER_REAL, 0)
                result["elapsed_seconds"] = round(time.monotonic() - started, 3)
            print(f"{case} attempt {repeat}: {result['status']} ({output})", flush=True)
            if result.get("detail"):
                print(steering_sanitize(result["detail"], key), flush=True)
            if interrupted:
                break
        if interrupted:
            break
    print("Raw observations retained in YAML; accepted input is not proof of application.")
    return 130 if interrupted else int(any(case["status"] not in {"observed", "observed_rejection"} for case in results))


if __name__ == "__main__":
    if sys.argv[1:2] == ["dependencies"]:
        record_dependencies()
    elif sys.argv[1:2] == ["steering"]:
        del sys.argv[1]
        raise SystemExit(steering_main())
    else:
        main()
