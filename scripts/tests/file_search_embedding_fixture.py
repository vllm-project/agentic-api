"""Deterministic embedding endpoint for the pgvector OpenAI-client integration test."""
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading


def embedding(text: str) -> list[float]:
    text = text.casefold()
    if "climate" in text:
        return [1.0, 0.0]
    if "office" in text or "opening schedule" in text:
        return [0.0, 1.0]
    return [-1.0, -1.0]


@contextmanager
def embedding_upstream(enabled: bool):
    if not enabled:
        yield None
        return

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            assert self.path == "/v1/embeddings", self.path
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            body = json.dumps({"object": "list", "model": request["model"], "data": [
                {"object": "embedding", "index": index, "embedding": embedding(text)}
                for index, text in enumerate(request["input"])
            ]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1"
        finally:
            server.shutdown()
            worker.join(timeout=5)
            assert not worker.is_alive(), "embedding fixture did not stop"
