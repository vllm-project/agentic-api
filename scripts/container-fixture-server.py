#!/usr/bin/env python3
"""Serve a recorded vLLM Responses API fixture for container integration tests.

The JSON response mirrors the existing agentic-server-core YAML cassette while
keeping the CI helper dependency-free.
"""

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--port", required=True, type=int)
    return parser.parse_args()


def make_handler(response_template: dict):
    import uuid

    class FixtureHandler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            if self.path == "/health":
                self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.send_error(404)

        def do_POST(self) -> None:
            if self.path != "/v1/responses":
                self.send_error(404)
                return

            try:
                content_length = int(self.headers.get("Content-Length", "0"))
                request = json.loads(self.rfile.read(content_length))
            except (ValueError, json.JSONDecodeError):
                self.send_error(400, "request body must be valid JSON")
                return

            input_items = request.get("input")
            expected_prompt = {
                "content": "Reply with exactly one word: HELLO",
                "role": "user",
                "type": "message",
            }
            if (
                not isinstance(input_items, list)
                or not input_items
                or input_items[-1] != expected_prompt
                or request.get("model") != "gpt-4o"
                or request.get("stream") is not False
            ):
                self.log_error("fixture request mismatch: %s", json.dumps(request, sort_keys=True))
                self.send_error(422, "request did not match the recorded fixture")
                return

            # Generate unique IDs per request to avoid duplicate key errors
            response = response_template.copy()
            response["id"] = f"resp_{uuid.uuid4().hex}"
            if response.get("output") and len(response["output"]) > 0:
                response["output"][0]["id"] = f"msg_{uuid.uuid4().hex}"

            response_body = json.dumps(response).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(response_body)))
            self.end_headers()
            self.wfile.write(response_body)

    return FixtureHandler


def main() -> None:
    args = parse_args()
    response_template = json.loads(args.fixture.read_text())
    server = ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(response_template))
    server.serve_forever()


if __name__ == "__main__":
    main()
