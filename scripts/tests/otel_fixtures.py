#!/usr/bin/env python3
"""Fixtures for the gateway telemetry smoke test and overhead benchmark.

Standard library only. Subcommands:

  upstream PORT                 serve a scripted OpenAI Responses upstream
  requests GATEWAY_PORT         send streamed, blocking, proxied, and WebSocket requests
  check SERVICE LOG METRICS     verify the Collector's debug log and Prometheus output

The metric allow-list is read from the Rust test harness, so the smoke test
and `cargo test` enforce the same cardinality contract.
"""

from __future__ import annotations

import base64
import http.client
import json
import os
import re
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / "crates/agentic-server-core/tests/execution_metrics/harness.rs"

USAGE = {
    "input_tokens": 7,
    "input_tokens_details": {"cached_tokens": 0},
    "output_tokens": 2,
    "output_tokens_details": {"reasoning_tokens": 0},
    "total_tokens": 9,
}
MESSAGE = {
    "id": "msg_up",
    "type": "message",
    "role": "assistant",
    "status": "completed",
    "content": [{"type": "output_text", "text": "hi", "annotations": []}],
}


def sse_body() -> bytes:
    events = [
        {"type": "response.created", "response": {"id": "resp_up", "status": "in_progress", "output": []}},
        {
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {**MESSAGE, "status": "in_progress", "content": []},
        },
        {"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "hi"},
        {"type": "response.output_item.done", "output_index": 0, "item": MESSAGE},
        {
            "type": "response.completed",
            "response": {"id": "resp_up", "status": "completed", "output": [MESSAGE], "usage": USAGE},
        },
    ]
    lines = [f"data: {json.dumps({**event, 'sequence_number': n})}\n\n" for n, event in enumerate(events)]
    return ("".join(lines) + "data: [DONE]\n\n").encode()


def json_body() -> bytes:
    body = {
        "id": "resp_up",
        "object": "response",
        "created_at": 0,
        "model": "test-model",
        "status": "completed",
        "output": [MESSAGE],
        "usage": USAGE,
    }
    return json.dumps(body).encode()


class Upstream(BaseHTTPRequestHandler):
    """Completes every Responses request; keeps connections alive."""

    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        # Headers and body are separate writes; without this, Nagle's algorithm
        # and delayed ACKs add ~40 ms to every keep-alive response.
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def do_GET(self) -> None:  # noqa: N802 - http.server API
        self._send(200, "application/json", b"{}")

    def do_POST(self) -> None:  # noqa: N802 - http.server API
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length) or b"{}")
        if request.get("stream"):
            self._send(200, "text/event-stream", sse_body())
        else:
            self._send(200, "application/json", json_body())

    def _send(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args: object) -> None:
        pass


def serve_upstream(port: int) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), Upstream)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def responses_request(stream: bool, store: bool = True) -> dict:
    return {"model": "test-model", "input": "say hi", "stream": stream, "store": store}


def post(connection: http.client.HTTPConnection, payload: dict) -> bytes:
    connection.request(
        "POST", "/v1/responses", body=json.dumps(payload), headers={"content-type": "application/json"}
    )
    response = connection.getresponse()
    body = response.read()
    if response.status != 200:
        raise RuntimeError(f"gateway answered {response.status}: {body[:200]!r}")
    return body


class WebSocket:
    """A minimal RFC 6455 client: text frames out, frames in."""

    def __init__(self, port: int, path: str = "/v1/responses") -> None:
        self.socket = socket.create_connection(("127.0.0.1", port))
        self.reader = self.socket.makefile("rb")
        key = base64.b64encode(os.urandom(16)).decode()
        self.socket.sendall(
            (
                f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n"
                f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        status = self.reader.readline()
        if b" 101 " not in status:
            raise RuntimeError(f"websocket upgrade failed: {status!r}")
        while self.reader.readline() not in (b"\r\n", b""):
            pass

    def send_text(self, text: str) -> None:
        self._send(0x1, text.encode())

    def _send(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        header = bytes([0x80 | opcode])
        if len(payload) < 126:
            header += bytes([0x80 | len(payload)])
        elif len(payload) < 1 << 16:
            header += bytes([0x80 | 126]) + len(payload).to_bytes(2, "big")
        else:
            header += bytes([0x80 | 127]) + len(payload).to_bytes(8, "big")
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.socket.sendall(header + mask + masked)

    def receive_json(self) -> dict:
        while True:
            first, second = self.reader.read(2)
            opcode, length = first & 0x0F, second & 0x7F
            if length == 126:
                length = int.from_bytes(self.reader.read(2), "big")
            elif length == 127:
                length = int.from_bytes(self.reader.read(8), "big")
            payload = self.reader.read(length)
            if opcode == 0x1:
                return json.loads(payload)
            if opcode == 0x9:
                self._send(0xA, payload)
            elif opcode == 0x8:
                raise RuntimeError("websocket closed by the gateway")

    def complete(self) -> None:
        """Send one response.create and read until it completes."""
        self.send_text(json.dumps({**responses_request(True, store=False), "type": "response.create"}))
        while (event := self.receive_json())["type"] != "response.completed":
            if event["type"] == "error":
                raise RuntimeError(f"websocket request failed: {event}")

    def close(self) -> None:
        self._send(0x8, b"")
        self.socket.close()


def send_smoke_requests(gateway_port: int) -> None:
    connection = http.client.HTTPConnection("127.0.0.1", gateway_port, timeout=30)
    body = post(connection, responses_request(stream=True))
    assert b"response.completed" in body, body[-200:]
    post(connection, responses_request(stream=False))
    post(connection, responses_request(stream=False, store=False))  # proxied
    connection.close()
    socket_ = WebSocket(gateway_port)
    socket_.complete()
    socket_.complete()
    socket_.close()


# ---------------------------------------------------------------- checking


def _rust_string_list(text: str) -> list[str]:
    return re.findall(r'"([^"]*)"', text)


def load_allow_list() -> tuple[dict[str, list[str]], dict[str, list[str]]]:
    """Parse INSTRUMENT_KEYS and KEY_VALUES from the Rust test harness."""
    source = HARNESS.read_text()
    tables = {}
    for name in ("INSTRUMENT_KEYS", "KEY_VALUES"):
        block = re.search(rf"const {name}: &\[\(&str, &\[&str\]\)\] = &\[(.*?)\n\];", source, re.S)
        if block is None:
            raise SystemExit(f"cannot find {name} in {HARNESS}")
        entries = re.findall(r'\(\s*"([^"]+)",\s*&\[(.*?)\],?\s*\)', block.group(1), re.S)
        tables[name] = {key: _rust_string_list(values) for key, values in entries}
    return tables["INSTRUMENT_KEYS"], tables["KEY_VALUES"]


def value_allowed(key_values: dict[str, list[str]], key: str, value: str) -> bool:
    if key in key_values:
        return value in key_values[key]
    if key == "http.route":
        return value.startswith("/") and "?" not in value
    if key == "http.response.status_code":
        return value.isdigit() and 100 <= int(value) <= 599
    return False


_ATTRIBUTE = re.compile(r"^\s*-> ([^:]+): (?:Str|Bool|Int|Double)\((.*)\)$")
_DATA_POINT = re.compile(r"^\w*DataPoints #\d+$")


def parse_debug_metrics(log: str) -> list[tuple[str | None, str | None, dict[str, str]]]:
    """(service.name, metric, attributes) for each data point in a detailed debug log.

    A data point's attribute block is printed only when it has attributes.
    """
    points: list[tuple[str | None, str | None, dict[str, str]]] = []
    service = metric = None
    section = None
    attributes: dict[str, str] = {}
    for raw in log.splitlines():
        line = raw.split("| ", 1)[1] if "| " in raw else raw
        stripped = line.strip()
        if stripped == "Resource attributes:":
            section, service = "resource", None
        elif stripped == "Descriptor:":
            section = "descriptor"
        elif _DATA_POINT.match(stripped):
            section, attributes = None, {}
            points.append((service, metric, attributes))
        elif stripped == "Data point attributes:":
            section = "point"
        elif match := _ATTRIBUTE.match(line):
            if section == "resource" and match.group(1) == "service.name":
                service = match.group(2)
            elif section == "point":
                attributes[match.group(1)] = match.group(2)
        elif stripped.startswith("-> Name: ") and section == "descriptor":
            metric = stripped.removeprefix("-> Name: ")
        elif not stripped.startswith("->") and section == "point":
            section = None
    return points


def check(service: str, log_path: str, prometheus_path: str) -> None:
    instrument_keys, key_values = load_allow_list()
    points = [(metric, attrs) for svc, metric, attrs in parse_debug_metrics(Path(log_path).read_text()) if svc == service]
    if not points:
        raise SystemExit(f"the Collector printed no metrics for service.name={service}")
    problems = []
    for metric, attributes in points:
        if metric not in instrument_keys:
            problems.append(f"unlisted instrument {metric}")
            continue
        for key, value in attributes.items():
            if key not in instrument_keys[metric]:
                problems.append(f"{metric}: unlisted attribute {key}")
            elif not value_allowed(key_values, key, value):
                problems.append(f"{metric}: {key}={value!r} outside the allowed values")
    seen = {metric for metric, _ in points}
    missing = sorted(set(instrument_keys) - seen)
    if missing:
        problems.append(f"instruments missing from the Collector output: {missing}")
    prometheus = Path(prometheus_path).read_text()
    if not re.search(rf'^agentic_execution_count\w*\{{[^}}]*service_name="{re.escape(service)}"', prometheus, re.M):
        problems.append("the Prometheus endpoint has no agentic_execution_count series for this service")
    if problems:
        raise SystemExit("telemetry smoke check failed:\n  " + "\n  ".join(problems))
    print(f"ok: {len(seen)} instruments, {len(points)} data points, all allow-listed, service.name={service}")


def main() -> None:
    command, *args = sys.argv[1:] or ["help"]
    if command == "upstream":
        serve_upstream(int(args[0]))
        threading.Event().wait()
    elif command == "requests":
        send_smoke_requests(int(args[0]))
    elif command == "check":
        check(*args)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
