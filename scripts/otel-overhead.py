#!/usr/bin/env python3
"""Measure gateway latency, CPU, and memory with telemetry disabled and enabled.

    cargo build --release -p agentic-server --bin agentic-server
    docker compose -f deploy/otel/docker-compose.yaml up -d
    scripts/otel-overhead.py --bin target/release/agentic-server

Runs one gateway per mode against the same in-process scripted upstream,
alternating modes for --rounds rounds. Each round sends --requests sequential
streamed and blocking Responses requests (store=true, so every request is
rehydrated, executed, and persisted) after --warmup unmeasured ones. With
telemetry enabled the gateway exports traces (every request sampled) and
metrics over OTLP/HTTP to --otlp-endpoint.

Reports per-request latency percentiles, gateway CPU time per request, and
the gateway's peak resident set size, as a Markdown table.

The client re-arms TCP_QUICKACK after every read so that the gateway's
streamed writes never wait for a delayed ACK (the gateway does not set
TCP_NODELAY on accepted sockets); latency then reflects the gateway's own work.
Linux only.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "tests"))

import otel_fixtures  # noqa: E402

TICKS = os.sysconf("SC_CLK_TCK")


def cpu_seconds(pid: int) -> float:
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / TICKS


def peak_rss_mib(pid: int) -> float:
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmHWM:"):
            return int(line.split()[1]) / 1024
    raise RuntimeError("no VmHWM")


def start_gateway(binary: str, upstream: int, enabled: bool, endpoint: str, home: str) -> tuple[subprocess.Popen, int]:
    port = otel_fixtures.free_port()
    env = {"PATH": os.environ["PATH"], "HOME": home, "AGENTIC_API_HOME": home, "RUST_LOG": "warn"}
    if enabled:
        env |= {
            "OTEL_TRACES_EXPORTER": "otlp",
            "OTEL_METRICS_EXPORTER": "otlp",
            "OTEL_EXPORTER_OTLP_ENDPOINT": endpoint,
            "OTEL_SERVICE_NAME": "agentic-overhead",
        }
    process = subprocess.Popen(
        [
            binary,
            "--llm-api-base",
            f"http://127.0.0.1:{upstream}",
            "--skip-llm-ready-check",
            "--gateway-host",
            "127.0.0.1",
            "--gateway-port",
            str(port),
        ],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(500):
        try:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
            connection.request("GET", "/health")
            if connection.getresponse().status == 200:
                return process, port
        except OSError:
            time.sleep(0.02)
    process.kill()
    raise RuntimeError("gateway did not start")


class QuickAckClient:
    """A keep-alive HTTP/1.1 client that acknowledges every segment at once."""

    def __init__(self, port: int) -> None:
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=30)
        self.socket.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buffer = b""

    def _receive(self) -> None:
        self.socket.setsockopt(socket.IPPROTO_TCP, socket.TCP_QUICKACK, 1)
        chunk = self.socket.recv(65536)
        if not chunk:
            raise RuntimeError("gateway closed the connection")
        self.buffer += chunk

    def post(self, payload: dict) -> bytes:
        body = json.dumps(payload).encode()
        self.socket.sendall(
            b"POST /v1/responses HTTP/1.1\r\nHost: gateway\r\nContent-Type: application/json\r\n"
            + f"Content-Length: {len(body)}\r\n\r\n".encode()
            + body
        )
        while b"\r\n\r\n" not in self.buffer:
            self._receive()
        head, self.buffer = self.buffer.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n", 1)[0]
        if b" 200 " not in status:
            raise RuntimeError(f"gateway answered {status!r}")
        headers = dict(line.split(b": ", 1) for line in head.split(b"\r\n")[1:])
        headers = {key.lower(): value for key, value in headers.items()}
        if b"content-length" in headers:
            length = int(headers[b"content-length"])
            while len(self.buffer) < length:
                self._receive()
            response, self.buffer = self.buffer[:length], self.buffer[length:]
            return response
        # Chunked: every gateway response ends with the zero-length chunk.
        while b"\r\n0\r\n\r\n" not in self.buffer:
            self._receive()
        response, self.buffer = self.buffer.split(b"\r\n0\r\n\r\n", 1)
        return response

    def close(self) -> None:
        self.socket.close()


def run_scenario(port: int, stream: bool, requests: int, warmup: int, pid: int) -> tuple[list[float], float]:
    client = QuickAckClient(port)
    payload = otel_fixtures.responses_request(stream=stream)
    for _ in range(warmup):
        client.post(payload)
    latencies = []
    cpu_before = cpu_seconds(pid)
    for _ in range(requests):
        started = time.perf_counter()
        client.post(payload)
        latencies.append(time.perf_counter() - started)
    cpu = (cpu_seconds(pid) - cpu_before) / requests
    client.close()
    return latencies, cpu


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bin", default="target/release/agentic-server")
    parser.add_argument("--requests", type=int, default=1000)
    parser.add_argument("--warmup", type=int, default=100)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--otlp-endpoint", default="http://127.0.0.1:4318")
    args = parser.parse_args()

    upstream_port = otel_fixtures.free_port()
    otel_fixtures.serve_upstream(upstream_port)
    results: dict[tuple[str, str], dict] = {}
    for round_ in range(args.rounds):
        for enabled in (False, True):
            mode = "enabled" if enabled else "disabled"
            with tempfile.TemporaryDirectory() as home:
                process, port = start_gateway(args.bin, upstream_port, enabled, args.otlp_endpoint, home)
                try:
                    for stream in (True, False):
                        scenario = "streaming" if stream else "non-streaming"
                        latencies, cpu = run_scenario(port, stream, args.requests, args.warmup, process.pid)
                        entry = results.setdefault((scenario, mode), {"latencies": [], "cpu": [], "rss": []})
                        entry["latencies"] += latencies
                        entry["cpu"].append(cpu)
                    results[("streaming", mode)]["rss"].append(peak_rss_mib(process.pid))
                finally:
                    process.send_signal(signal.SIGTERM)
                    process.wait(timeout=30)
            print(f"round {round_ + 1}/{args.rounds}: {mode} done", file=sys.stderr)

    print("| Scenario | Telemetry | p50 (ms) | p90 (ms) | p99 (ms) | CPU / request (ms) | Peak RSS (MiB) |")
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: |")
    for scenario in ("streaming", "non-streaming"):
        for mode in ("disabled", "enabled"):
            entry = results[(scenario, mode)]
            latencies = entry["latencies"]
            rss = results[("streaming", mode)]["rss"]
            print(
                f"| {scenario} | {mode} "
                f"| {percentile(latencies, 0.5) * 1e3:.2f} "
                f"| {percentile(latencies, 0.9) * 1e3:.2f} "
                f"| {percentile(latencies, 0.99) * 1e3:.2f} "
                f"| {statistics.median(entry['cpu']) * 1e3:.3f} "
                f"| {statistics.median(rss):.1f} |"
            )
    print(json.dumps({"requests": args.requests, "rounds": args.rounds}), file=sys.stderr)


if __name__ == "__main__":
    main()
