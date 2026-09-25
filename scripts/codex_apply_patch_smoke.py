#!/usr/bin/env python3
"""Verify actual Codex applies a patch through the gateway's custom-tool adapter.

The upstream is synthetic: this verifies the client/tool protocol and file edit,
not a live model's ability to follow the patch grammar.
"""

from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
MODEL = "codex-patch-test"
PATCH = "*** Begin Patch\n*** Update File: patch.txt\n@@\n-before\n+CODEX_PATCH_OK\n*** End Patch\n"
CALL_ID = "call_patch_1"


class MockUpstream(ThreadingHTTPServer):
    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.requests: list[dict] = []


class Handler(BaseHTTPRequestHandler):
    server: MockUpstream

    def log_message(self, *_args: object) -> None:
        pass

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            body = b"{}"
        elif self.path == "/v1/models":
            body = json.dumps({"object": "list", "data": [{"id": MODEL, "max_model_len": 131072}]}).encode()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/v1/responses":
            self.send_error(404)
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(request)
        resolved = any(
            item.get("type") == "function_call_output" and item.get("call_id") == CALL_ID
            for item in request.get("input", []) if isinstance(item, dict)
        )
        response = {
            "id": "resp_patch_done" if resolved else "resp_patch_call", "object": "response",
            "created_at": 0, "model": MODEL, "status": "in_progress", "output": [],
        }
        events = [{"type": "response.created", "response": dict(response)}]
        if resolved:
            item = {
                "id": "msg_patch_done", "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "CODEX_PATCH_OK", "annotations": []}],
            }
            events += [
                {"type": "response.output_item.added", "output_index": 0,
                 "item": {**item, "status": "in_progress", "content": []}},
                {"type": "response.content_part.added", "output_index": 0, "item_id": item["id"],
                 "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}},
                {"type": "response.output_text.delta", "output_index": 0, "item_id": item["id"],
                 "content_index": 0, "delta": "CODEX_PATCH_OK"},
                {"type": "response.output_text.done", "output_index": 0, "item_id": item["id"],
                 "content_index": 0, "text": "CODEX_PATCH_OK"},
                {"type": "response.content_part.done", "output_index": 0, "item_id": item["id"],
                 "content_index": 0, "part": item["content"][0]},
            ]
        else:
            arguments = json.dumps({"input": PATCH})
            item = {"id": "fc_patch_1", "type": "function_call", "name": "apply_patch",
                    "call_id": CALL_ID, "status": "completed", "arguments": arguments}
            events.append({"type": "response.output_item.added", "output_index": 0,
                           "item": {**item, "status": "in_progress", "arguments": ""}})
            # Split inside JSON escapes as well as ordinary patch text.
            for offset in range(0, len(arguments), 7):
                events.append({"type": "response.function_call_arguments.delta", "output_index": 0,
                               "item_id": item["id"], "delta": arguments[offset:offset + 7]})
            events.append({"type": "response.function_call_arguments.done", "output_index": 0,
                           "item_id": item["id"], "name": "apply_patch", "arguments": arguments})
        events.append({"type": "response.output_item.done", "output_index": 0, "item": item})
        response.update(status="completed", output=[item])
        events.append({"type": "response.completed", "response": response})
        body = "".join(
            f"event: {event['type']}\ndata: {json.dumps({**event, 'sequence_number': index})}\n\n"
            for index, event in enumerate(events)
        ).encode() + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def run_case(websocket: bool) -> None:
    label = "websocket" if websocket else "http"
    with tempfile.TemporaryDirectory(prefix=f"agentic-codex-patch-{label}-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        (workspace / "patch.txt").write_text("before\n")
        codex_home = root / "codex"
        codex_home.mkdir()
        env = {key: os.environ[key] for key in ("PATH", "HOME", "LANG", "LD_LIBRARY_PATH") if key in os.environ}
        env.update(CODEX_HOME=str(codex_home), AGENTIC_API_HOME=str(root / "agentic"))
        env["RUST_LOG"] = "info,agentic_server::handler::websocket=debug"
        binary = Path(os.environ.get("AGENTIC_SERVER_BIN", ROOT / "target/debug/agentic-server")).resolve()
        codex = shutil.which(os.environ.get("CODEX_BIN", "codex"))
        assert codex, "Codex executable not found"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        upstream = MockUpstream()
        thread = threading.Thread(target=upstream.serve_forever)
        thread.start()
        gateway = None
        try:
            with (root / "gateway.log").open("w+") as log:
                gateway = subprocess.Popen([
                    str(binary), "--gateway-host", "127.0.0.1", "--gateway-port", str(port),
                    "--llm-api-base", f"http://127.0.0.1:{upstream.server_port}",
                    "--skip-llm-ready-check", "--db-url", f"sqlite://{root / 'gateway.db'}",
                ], env=env, stdout=log, stderr=subprocess.STDOUT)
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                base = f"http://127.0.0.1:{port}"
                deadline = time.monotonic() + 30
                while True:
                    assert gateway.poll() is None, "gateway exited during startup"
                    try:
                        with opener.open(base + "/health", timeout=1):
                            break
                    except (urllib.error.URLError, TimeoutError):
                        assert time.monotonic() < deadline, "gateway startup timed out"
                        time.sleep(0.1)
                version = subprocess.check_output([codex, "--version"], env=env, text=True).split()[-1]
                with opener.open(base + "/v1/models?client_version=" + version, timeout=5) as response:
                    catalog = json.load(response)
                assert catalog["models"][0]["apply_patch_tool_type"] == "freeform"
                catalog_path = codex_home / "model_catalog.json"
                catalog_path.write_text(json.dumps(catalog))
                (codex_home / "config.toml").write_text(
                    f'model = "{MODEL}"\nmodel_provider = "patch-test"\n'
                    f'model_catalog_json = {json.dumps(str(catalog_path))}\n'
                    '[model_providers.patch-test]\nname = "Patch test"\n'
                    f'base_url = "{base}/v1"\nwire_api = "responses"\nrequires_openai_auth = false\n'
                    f'supports_websockets = {str(websocket).lower()}\n'
                )
                result = subprocess.run([
                    codex, "exec", "--skip-git-repo-check", "--sandbox", "workspace-write",
                    "-c", 'approval_policy="never"', "--disable", "apps", "--disable", "plugins",
                    "--disable", "image_generation", "-C", str(workspace), "--json",
                    "Use apply_patch to replace before in patch.txt with CODEX_PATCH_OK. Then reply CODEX_PATCH_OK.",
                ], env=env, cwd=workspace, capture_output=True, text=True, timeout=60)
                assert result.returncode == 0, result.stdout + result.stderr
                assert (workspace / "patch.txt").read_text() == "CODEX_PATCH_OK\n", result.stdout + result.stderr
                assert len(upstream.requests) == 2, upstream.requests
                for request in upstream.requests:
                    tool = next(tool for tool in request["tools"] if tool["name"] == "apply_patch")
                    assert tool["type"] == "function"
                    assert "*** Begin Patch" in tool["description"]
                    assert "grammar" in tool["description"]
                    assert tool["parameters"]["properties"]["input"]["type"] == "string"
                inputs = upstream.requests[1]["input"]
                call = next(item for item in inputs if item.get("type") == "function_call" and item["call_id"] == CALL_ID)
                assert json.loads(call["arguments"])["input"] == PATCH
                output = next(item for item in inputs if item.get("type") == "function_call_output" and item["call_id"] == CALL_ID)
                assert "patch.txt" in json.dumps(output["output"]), output
                gateway_log = (root / "gateway.log").read_text()
                used_websocket = "accepted websocket response.create" in gateway_log
                assert used_websocket == websocket, gateway_log
                print(f"Codex {version} {label}: native apply_patch edited the file and continued successfully")
        except Exception:
            if (root / "gateway.log").exists():
                print((root / "gateway.log").read_text(), file=sys.stderr)
            raise
        finally:
            if gateway is not None and gateway.poll() is None:
                gateway.terminate()
                try:
                    gateway.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    gateway.kill()
                    gateway.wait(timeout=5)
            upstream.shutdown()
            upstream.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive(), "mock upstream failed to stop"


if __name__ == "__main__":
    for websocket in (False, True):
        run_case(websocket)
