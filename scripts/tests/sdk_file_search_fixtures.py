"""Loopback-only deterministic retrieval models for independent SDK contracts."""

import os
import socket
import subprocess
import time

import httpx2 as httpx
from openai.types.responses import Response

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import threading


class RetrievalModels:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []
        self.block = None
        self.entered = threading.Event()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def setup(self):
                super().setup()
                self.connection.settimeout(5)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                if length < 0 or length > 4 * 1024 * 1024:
                    self.send_error(413)
                    return
                request = json.loads(self.rfile.read(length))
                with fixture.lock:
                    fixture.requests.append((self.path, request))
                    block = fixture.block
                if self.path == "/v1/embeddings":
                    if block is not None:
                        fixture.entered.set()
                        if not block.wait(10):
                            self.send_error(504)
                            return
                    inputs = request["input"]
                    if isinstance(inputs, str):
                        inputs = [inputs]
                    response = {
                        "object": "list",
                        "model": request["model"],
                        "data": [
                            {
                                "object": "embedding",
                                "index": index,
                                "embedding": [1.0, 0.0],
                            }
                            for index, _ in enumerate(inputs)
                        ],
                        "usage": {"prompt_tokens": 1, "total_tokens": 1},
                    }
                elif self.path == "/v1/chat/completions":
                    response = {
                        "id": "chatcmpl-local-rewrite",
                        "object": "chat.completion",
                        "created": 0,
                        "model": request["model"],
                        "choices": [
                            {
                                "index": 0,
                                "message": {
                                    "role": "assistant",
                                    "content": "lunar return policy",
                                },
                                "finish_reason": "stop",
                            }
                        ],
                    }
                elif self.path == "/rerank":
                    response = {
                        "id": "rerank-local",
                        "model": request["model"],
                        "results": [
                            {"index": index, "relevance_score": 1.0 / (index + 1)}
                            for index, _ in enumerate(request["documents"])
                        ],
                    }
                else:
                    self.send_error(404)
                    return
                body = json.dumps(response).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # A cancelled ingestion deliberately drops its model request.

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = False
        self.thread = threading.Thread(
            target=self.server.serve_forever, name="sdk-retrieval-models"
        )

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.unblock()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise RuntimeError("Retrieval model fixture did not stop")

    def block_embeddings(self):
        with self.lock:
            self.entered.clear()
            self.block = threading.Event()

    def unblock(self):
        with self.lock:
            if self.block is not None:
                self.block.set()
                self.block = None

    def snapshot(self):
        with self.lock:
            return list(self.requests)

    def configure(self, directory):
        base = f"http://127.0.0.1:{self.server.server_port}/v1"
        Path(directory, "config.toml").write_text(f'''[file_search.vector_stores]
default_provider_id = "sdk"

[file_search.vector_stores.providers.sdk]
base_url = "{base}"
models = ["embedding", "rewrite", "rerank"]
protocol = "vllm"
score_interpretation = "probability"

[file_search.vector_stores.default_embedding_model]
provider_id = "sdk"
model_id = "embedding"
embedding_dimensions = 2

[file_search.vector_stores.default_reranker_model]
provider_id = "sdk"
model_id = "rerank"

[file_search.vector_stores.rewrite_query_params]
model = {{ provider_id = "sdk", model_id = "rewrite" }}
max_tokens = 100
temperature = 0.0

[file_search.vector_stores.file_batch_params]
cleanup_interval_seconds = 1
''')


class ResponsesModel:
    def __init__(self):
        self.file_id = None
        self.requests = []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def setup(self):
                super().setup()
                self.connection.settimeout(5)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                if self.path != "/v1/responses" or not 0 < length <= 4 * 1024 * 1024:
                    self.send_error(400)
                    return
                request = json.loads(self.rfile.read(length))
                fixture.requests.append(request)
                tool_output = any(
                    item.get("type") == "function_call_output"
                    for item in request["input"]
                )
                if tool_output:
                    text = (
                        f"Returns are accepted for thirty days. 【{fixture.file_id}】"
                    )
                    item = {
                        "type": "message",
                        "id": "msg_sdk",
                        "role": "assistant",
                        "status": "completed",
                        "content": [
                            {"type": "output_text", "text": text, "annotations": []}
                        ],
                    }
                else:
                    item = {
                        "type": "function_call",
                        "id": "fc_sdk",
                        "call_id": "call_sdk",
                        "name": "file_search",
                        "arguments": json.dumps({"queries": ["lunar"]}),
                        "status": "completed",
                    }
                response = {
                    "id": "resp_fixture",
                    "object": "response",
                    "created_at": 0,
                    "model": "test-model",
                    "status": "completed",
                    "output": [item],
                    "parallel_tool_calls": request.get("parallel_tool_calls", True),
                    "tool_choice": "auto",
                    "tools": request.get("tools", []),
                }
                Response.model_validate(response)
                if not request.get("stream"):
                    payload = json.dumps(response).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                pending = dict(response, status="in_progress", output=[])
                events = [
                    {"type": "response.created", "response": pending},
                    {"type": "response.in_progress", "response": pending},
                ]
                added = dict(item, status="in_progress")
                if tool_output:
                    added["content"] = []
                events.append(
                    {
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": added,
                    }
                )
                if tool_output:
                    indexes = {
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": item["id"],
                    }
                    events.append(
                        {
                            "type": "response.content_part.added",
                            **indexes,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": [],
                            },
                        }
                    )
                    split = text.index("【") + 7
                    for delta in (text[:split], text[split:]):
                        events.append(
                            {
                                "type": "response.output_text.delta",
                                **indexes,
                                "delta": delta,
                                "logprobs": [],
                            }
                        )
                    events.append(
                        {
                            "type": "response.output_text.done",
                            **indexes,
                            "text": text,
                            "logprobs": [],
                        }
                    )
                    events.append(
                        {
                            "type": "response.content_part.done",
                            **indexes,
                            "part": item["content"][0],
                        }
                    )
                events.append(
                    {
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": item,
                    }
                )
                events.append({"type": "response.completed", "response": response})
                for sequence, event in enumerate(events):
                    event["sequence_number"] = sequence
                payload = (
                    "".join(f"data: {json.dumps(event)}\n\n" for event in events)
                    + "data: [DONE]\n\n"
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = False
        self.thread = threading.Thread(
            target=self.server.serve_forever, name="sdk-responses-model"
        )

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise RuntimeError("Responses fixture failed to stop")


class Gateway:
    """Own one loopback process; reuse its SQL/files state for restart coverage."""

    def __init__(self, binary, directory, upstream):
        self.binary = str(Path(binary).resolve())
        self.directory = directory
        self.upstream = upstream
        self.process = None
        self.log = None
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            self.port = listener.getsockname()[1]
        self.base = f"http://127.0.0.1:{self.port}"

    def start(self):
        assert self.process is None
        env = {
            "PATH": os.environ.get("PATH", ""),
            "AGENTIC_API_HOME": self.directory,
            "AGENTIC_FILES_STORAGE_DIR": f"{self.directory}/files",
            "RUST_LOG": "warn",
        }
        self.log = open(f"{self.directory}/server.log", "a+")
        self.process = subprocess.Popen(
            [
                self.binary,
                "--llm-api-base",
                f"http://127.0.0.1:{self.upstream.server.server_port}/v1",
                "--skip-llm-ready-check",
                "--gateway-host",
                "127.0.0.1",
                "--gateway-port",
                str(self.port),
            ],
            env=env,
            stdout=self.log,
            stderr=self.log,
        )
        with httpx.Client(timeout=1, trust_env=False) as health:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    self.log.seek(0)
                    raise RuntimeError(self.log.read())
                try:
                    if health.get(f"{self.base}/health").is_success:
                        return
                except httpx.TransportError:
                    pass
                time.sleep(0.03)
        raise RuntimeError("Server startup deadline exceeded")

    def crash(self):
        """Deliberate crash recovery test; never used by graceful-shutdown checks."""
        process, self.process = self.process, None
        assert process is not None and process.poll() is None
        try:
            process.kill()
            if process.wait(timeout=5) >= 0:
                raise RuntimeError("Crash fixture did not terminate by signal")
        finally:
            self.log.close()

    def stop(self):
        process, self.process = self.process, None
        if process is None:
            return
        try:
            process.terminate()
            try:
                code = process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise RuntimeError(
                    "Server failed the graceful shutdown deadline"
                ) from None
            if code != 0:
                self.log.seek(0)
                raise RuntimeError(
                    f"Server did not exit cleanly ({code}): {self.log.read()}"
                )
        finally:
            self.log.close()

    def __enter__(self):
        try:
            self.start()
        except BaseException:
            self.stop()
            raise
        return self

    def __exit__(self, *_args):
        self.stop()
