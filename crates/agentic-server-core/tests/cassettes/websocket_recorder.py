"""Persistent duplex recording and bounded edge probes; no synthesized server events."""
from __future__ import annotations

import argparse
import base64
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


if __name__ == "__main__":
    if sys.argv[1:2] == ["dependencies"]:
        record_dependencies()
    else:
        main()
