#!/usr/bin/env python3
"""Exercise the real Claude Code CLI against the gateway and a deterministic Messages backend.

This verifies wire compatibility, tool-result history, and database-backed thread
recovery; it does not evaluate a live model. The trace records protocol metadata,
never authentication headers or prompt contents. Requires a built gateway and claude.
"""

from __future__ import annotations

import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request

BETA = "message-threads-2026-08-12"
MARKER = "CLAUDE_MESSAGES_TOOL_PROOF_91827"


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, handler, **state):
        super().__init__(("127.0.0.1", 0), handler)
        self.state = state
        self.records = []
        self.worker = threading.Thread(target=self.serve_forever, daemon=True)
        self.worker.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_port}"

    def close(self):
        self.shutdown()
        self.server_close()
        self.worker.join(timeout=5)
        assert not self.worker.is_alive()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def json(self, body):
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Backend(Handler):
    def do_GET(self):
        if self.path == "/health":
            self.json({"status": "ok"})
        elif self.path == "/v1/models":
            self.json({"object": "list", "data": [{"id": "cli-model", "object": "model", "owned_by": "fixture"}]})
        else:
            self.send_error(404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path.startswith("/v1/messages/count_tokens"):
            self.json({"input_tokens": 100})
            return
        if not self.path.startswith("/v1/messages"):
            self.send_error(404)
            return
        assert "thread" not in body, "local thread directives must not reach the stateless upstream"
        self.server.records.append(body)
        messages = body["messages"]
        is_probe = messages[0]["content"] == "THREAD_DATABASE_PROBE"
        if is_probe:
            text = "THREAD_PROBE_CREATED" if len(messages) == 1 else "THREAD_PROBE_RECOVERED"
            if len(messages) > 1:
                assert messages[1]["role"] == "assistant"
                assert messages[1]["content"][0]["text"] == "THREAD_PROBE_CREATED"
            content = [{"type": "text", "text": text}]
        else:
            results = [block for message in messages if isinstance(message["content"], list)
                       for block in message["content"] if block.get("type") == "tool_result"
                       and block.get("tool_use_id") == "call_cli_read"]
            if results:
                assert MARKER in json.dumps(results), "CLI must execute Read and return the fixture contents"
                content = [{"type": "text", "text": MARKER}]
            else:
                assert any(tool["name"] == "Read" for tool in body.get("tools", [])), "real CLI must declare Read"
                content = [{"type": "tool_use", "id": "call_cli_read", "name": "Read",
                            "input": {"file_path": self.server.state["fixture"]}}]
        stop = "tool_use" if content[0]["type"] == "tool_use" else "end_turn"
        message = {"id": f"msg_fixture_{len(self.server.records)}", "type": "message", "role": "assistant",
                   "model": body["model"], "content": content, "stop_reason": stop, "stop_sequence": None,
                   "usage": {"input_tokens": 100, "output_tokens": 20}}
        if not body.get("stream"):
            self.json(message)
            return
        start = dict(message, content=[], stop_reason=None, usage={"input_tokens": 100, "output_tokens": 0})
        events = [{"type": "message_start", "message": start}]
        for index, block in enumerate(content):
            if block["type"] == "tool_use":
                skeleton = dict(block, input={})
                delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
            else:
                skeleton = dict(block, text="")
                delta = {"type": "text_delta", "text": block["text"]}
            events.extend([{"type": "content_block_start", "index": index, "content_block": skeleton},
                           {"type": "content_block_delta", "index": index, "delta": delta},
                           {"type": "content_block_stop", "index": index}])
        events.extend([{"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                        "usage": {"output_tokens": 20}}, {"type": "message_stop"}])
        encoded = "".join(f'event: {event["type"]}\ndata: {json.dumps(event)}\n\n' for event in events).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


class Recorder(Handler):
    def forward(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        data = json.loads(body) if body else {}
        record = {"method": self.command, "path": self.path, "http_version": self.request_version,
                  "upgrade": self.headers.get("upgrade"), "thread": data.get("thread"),
                  "stream": data.get("stream"), "messages": len(data.get("messages", [])),
                  "beta": self.headers.get("anthropic-beta"), "origin": self.headers.get("x-test-origin", "claude-cli")}
        record["message_shapes"] = [
            {"role": message["role"], "blocks": [block.get("type") for block in message["content"]]
             if isinstance(message["content"], list) else ["text"]}
            for message in data.get("messages", [])]
        record["has_system"] = "system" in data
        record["has_tools"] = "tools" in data
        system = data.get("system")
        record["system_shape"] = ([{"type": block.get("type"), "keys": sorted(block),
                                    "cli_attribution": str(block.get("text", "")).startswith("x-anthropic-billing-header:")}
                                   for block in system]
                                  if isinstance(system, list) else type(system).__name__)
        previous_id = (data.get("thread") or {}).get("previous_message_id")
        previous = self.server.state.setdefault("created_parameters", {}).get(previous_id)
        if previous is not None:
            record["system_matches_create"] = system == previous.get("system")
            record["tools_match_create"] = data.get("tools") == previous.get("tools")
        self.server.records.append(record)
        connection = http.client.HTTPConnection("127.0.0.1", self.server.state["gateway_port"], timeout=90)
        try:
            headers = {key: value for key, value in self.headers.items()
                       if key.lower() not in {"host", "connection", "transfer-encoding"}}
            connection.request(self.command, self.path, body, headers)
            response = connection.getresponse()
            record["response_type"] = response.getheader("content-type")
            record["status"] = response.status
            payload = response.read()
            if record["response_type"].startswith("text/event-stream"):
                for line in payload.splitlines():
                    if line.startswith(b"data: "):
                        event = json.loads(line[6:])
                        if event.get("type") == "message_start":
                            record["response_message_id"] = event["message"]["id"]
                        elif event.get("type") == "error":
                            record["error"] = event.get("error")
            elif record["response_type"].startswith("application/json"):
                response_body = json.loads(payload)
                record["response_message_id"] = response_body.get("id")
                if response.status >= 400:
                    record["error"] = response_body.get("error")
            if data.get("thread") == {"type": "create"} and record.get("response_message_id"):
                self.server.state["created_parameters"][record["response_message_id"]] = {
                    key: data[key] for key in ("system", "tools") if key in data}
            self.send_response(response.status)
            for key, value in response.getheaders():
                if key.lower() not in {"content-length", "transfer-encoding", "connection"}:
                    self.send_header(key, value)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        finally:
            connection.close()

    do_GET = forward
    do_POST = forward


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway", type=Path, default=Path("target/debug/agentic-server"))
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--database-url", help="An isolated test database; defaults to a temporary SQLite file")
    parser.add_argument("--trace-file", type=Path)
    parser.add_argument("--full-cli", action="store_true", help="Run normal CLI startup instead of --bare; settings and home remain isolated")
    parser.add_argument("--require-cli-threads", action="store_true", help="Fail unless the real CLI continues with only the new tool-result turn")
    parser.add_argument("--experimental-cli-threads", action="store_true", help="Opt into the installed CLI's undocumented CLAUDE_CODE_TETHER_LIVE switch for this child process")
    parser.add_argument("--experimental-cli-inherited-fields", action="store_true", help="Also enable the CLI's internal inherited-field gate in disposable config; implies --experimental-cli-threads")
    args = parser.parse_args()
    if args.experimental_cli_inherited_fields:
        args.experimental_cli_threads = True
    assert args.claude, "claude must be installed"
    gateway_binary = args.gateway.resolve()
    with tempfile.TemporaryDirectory(prefix="messages-claude-e2e-") as temporary:
        work = Path(temporary)
        fixture = work / "proof.txt"
        fixture.write_text(MARKER)
        backend = Server(Backend, fixture=str(fixture))
        gateway_port = free_port()
        recorder = Server(Recorder, gateway_port=gateway_port)
        database_url = args.database_url or f"sqlite://{work / 'messages.db'}"
        gateway_env = os.environ.copy()
        for key in ["OIDC_ISSUER", "OIDC_AUDIENCE", "AGENTIC_API_SCHEMA_READY"]:
            gateway_env.pop(key, None)
        gateway_env["AGENTIC_API_HOME"] = str(work / "gateway-home")
        log = (work / "gateway.log").open("w+")

        def launch():
            process = subprocess.Popen([str(gateway_binary), "--llm-api-base", backend.url,
                                        "--gateway-host", "127.0.0.1", "--gateway-port", str(gateway_port),
                                        "--db-url", database_url], env=gateway_env, stdout=log, stderr=log)
            for _ in range(100):
                if process.poll() is not None:
                    log.seek(0)
                    raise AssertionError(f"gateway exited: {log.read()}")
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{gateway_port}/health", timeout=1).close()
                    assert (work / "gateway-home" / "config.toml").is_file(), "gateway config escaped its temporary home"
                    return process
                except OSError:
                    time.sleep(0.05)
            stop(process)
            raise AssertionError("gateway did not become healthy")

        gateway = None
        try:
            gateway = launch()

            def probe(thread, messages):
                data = {"model": "cli-model", "max_tokens": 64, "messages": messages, "thread": thread}
                request = urllib.request.Request(f"{recorder.url}/v1/messages", json.dumps(data).encode(),
                                                  {"content-type": "application/json", "anthropic-beta": BETA,
                                                   "x-api-key": "cli-fixture-key", "x-test-origin": "database-probe"})
                with urllib.request.urlopen(request, timeout=10) as response:
                    return json.load(response)

            first = probe({"type": "create"}, [{"role": "user", "content": "THREAD_DATABASE_PROBE"}])
            stop(gateway)
            gateway = launch()
            second = probe({"type": "continue", "previous_message_id": first["id"]},
                           [{"role": "user", "content": "Recover the earlier turn"}])
            assert second["content"][0]["text"] == "THREAD_PROBE_RECOVERED"
            cli_env = os.environ.copy()
            for key in ["ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX",
                        "CLAUDE_CODE_USE_FOUNDRY", "CLAUDECODE", "ANTHROPIC_CUSTOM_HEADERS",
                        "CLAUDE_CODE_TETHER_LIVE", "CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF"]:
                cli_env.pop(key, None)
            cli_env.update({"CLAUDE_CONFIG_DIR": str(work / "claude-home"), "ANTHROPIC_BASE_URL": recorder.url,
                            "ANTHROPIC_API_KEY": "cli-fixture-key", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                            "MAX_THINKING_TOKENS": "0", "CLAUDE_CODE_MAX_OUTPUT_TOKENS": "256"})
            if args.experimental_cli_threads:
                cli_env["CLAUDE_CODE_TETHER_LIVE"] = "1"
            if args.experimental_cli_inherited_fields:
                cli_env["CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF"] = "1"
                cli_env["DISABLE_GROWTHBOOK"] = "0"
                config = work / "claude-home"
                config.mkdir()
                (config / ".claude.json").write_text(json.dumps({
                    "cachedGrowthBookFeatures": {"tengu_quiet_heron": True}}))
            command = [args.claude, *([] if args.full_cli else ["--bare"]), "--print", "--model", "claude-sonnet-4-5-20250929",
                       "--tools", "Read", "--allowedTools", "Read", "--max-turns", "4",
                       "--no-session-persistence", "--output-format", "json", "--betas", BETA,
                       "--", f"Read {fixture} and return the exact marker in that file."]
            result = subprocess.run(command, env=cli_env, cwd=work, text=True, capture_output=True, timeout=90)
            assert result.returncode == 0, f"Claude CLI failed: {result.stdout}\n{result.stderr}"
            response = json.loads(result.stdout)
            assert response.get("result") == MARKER, response
            traffic = [record for record in recorder.records if record["origin"] == "claude-cli"]
            messages = [record for record in traffic if record["path"].split("?")[0] == "/v1/messages"]
            assert len(messages) >= 2, traffic
            incremental_pairs = [
                previous.get("thread") == {"type": "create"}
                and current.get("thread") == {"type": "continue", "previous_message_id": previous.get("response_message_id")}
                and current["message_shapes"] == [{"role": "user", "blocks": ["tool_result"]}]
                for previous, current in zip(messages, messages[1:])]
            sent_incremental = any(incremental_pairs)
            incremental = len(messages) == 2 and any(
                matched and previous["status"] == current["status"] == 200
                for matched, (previous, current) in zip(incremental_pairs, zip(messages, messages[1:])))
            trace = {"cli_version": subprocess.check_output([args.claude, "--version"], text=True).strip(),
                     "cli_mode": "normal" if args.full_cli else "bare",
                     "experimental_cli_threads": args.experimental_cli_threads,
                     "experimental_cli_inherited_fields": args.experimental_cli_inherited_fields,
                     "backend": "postgres" if database_url.startswith(("postgres:", "postgresql:")) else "sqlite",
                     "database_restart_recovery": True, "real_cli_read_tool": True,
                     "cli_used_threads": any(record["thread"] for record in messages),
                     "cli_sent_incremental_turn": sent_incremental,
                     "cli_incremental_turn_verified": incremental, "requests": recorder.records}
            if args.trace_file:
                args.trace_file.write_text(json.dumps(trace, indent=2) + "\n")
            print(json.dumps(trace, indent=2))
            assert all(record["response_type"].startswith("text/event-stream")
                       for record in messages if record["status"] == 200), traffic
            assert all(not record["upgrade"] for record in traffic), traffic
            if args.require_cli_threads:
                assert incremental, "Claude CLI did not successfully continue with only the new tool-result turn; inspect the trace"
        finally:
            if gateway is not None:
                stop(gateway)
            recorder.close()
            backend.close()
            log.close()


if __name__ == "__main__":
    main()
