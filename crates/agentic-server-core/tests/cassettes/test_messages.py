"""Local tests: existing recordings and synthetic fault injection, no GPU or new cassettes."""
import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import unittest

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
import validate_messages as check


class MessagesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "sample.yaml"
        self.doc = yaml.safe_load((check.RECORDINGS / "messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml").read_text())

    def validate(self):
        self.path.write_text(yaml.safe_dump(self.doc))
        return check.validate(self.path)

    def test_existing_nonstreaming_capture(self):
        self.validate()

    def test_invalid_scenarios(self):
        original = copy.deepcopy(self.doc)
        mutations = {
            "missing round": lambda d: d["turns"].pop(),
            "upstream failure": lambda d: d["turns"][0]["response"].update(status_code=500),
            "wrong tool id": lambda d: d["turns"][1]["request"]["body"]["messages"][2]["content"][0].update(tool_use_id="wrong"),
            "lost history": lambda d: d["turns"][1]["request"]["body"]["messages"].pop(1),
            "wrong tool output": lambda d: d["turns"][1]["request"]["body"]["messages"][2]["content"][0].update(content="wrong"),
            "unfinished": lambda d: d["turns"][1]["response"]["body"].update(stop_reason="max_tokens"),
            "no tool": lambda d: d["turns"][0]["response"]["body"].update(stop_reason="end_turn"),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                self.doc = copy.deepcopy(original)
                mutate(self.doc)
                with self.assertRaises(ValueError):
                    self.validate()

    def streaming_turn(self):
        doc = yaml.safe_load((check.RECORDINGS / "messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml").read_text())
        return doc["turns"][0]

    def test_preserves_captured_signature(self):
        message = check.response_message(self.streaming_turn())
        self.assertEqual(message["content"][0]["signature"], "32d0a706de774281bec15280dec400c0")

    def test_invalid_streaming_tool_arguments_fail_recording(self):
        turn = self.streaming_turn()
        raw = "".join(turn["response"]["sse"])
        lines = []
        for line in raw.splitlines(keepends=True):
            if line.startswith("data:"):
                event = json.loads(line[5:])
                if event.get("delta", {}).get("type") == "input_json_delta":
                    event["delta"]["partial_json"] = "{broken"
                line = "data: " + json.dumps(event) + "\n"
            lines.append(line)
        broken = copy.deepcopy(turn)
        broken["response"]["sse"] = ["".join(lines)]
        with self.assertRaises(json.JSONDecodeError):
            check.response_message(broken)

    def test_real_recorder_script_and_failure_preserves_files(self):
        captures = {}
        for suffix in ("streaming", "nonstreaming"):
            captures[suffix] = yaml.safe_load((check.RECORDINGS / f"messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-{suffix}.yaml").read_text())["turns"]
        fail = [False]

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                self.send_response(500 if fail[0] and body["stream"] else 200)
                self.send_header("Content-Type", "text/event-stream" if body["stream"] else "application/json")
                self.end_headers()
                if fail[0] and body["stream"]:
                    self.wfile.write(b"failed")
                    return
                turn = captures["streaming" if body["stream"] else "nonstreaming"][0 if len(body["messages"]) == 1 else 1]
                wire = "".join(turn["response"]["sse"]) if body["stream"] else json.dumps(turn["response"]["body"])
                self.wfile.write(wire.encode())

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with socket.socket() as port:
                port.bind(("127.0.0.1", 0))
                proxy_port = port.getsockname()[1]
            output = Path(self.temp.name) / "recordings"
            env = dict(os.environ, PYTHON=sys.executable, MODEL="qwen3", OUTPUT_DIR=str(output),
                       DYNAMO_URL=f"http://127.0.0.1:{server.server_port}", PROXY_PORT=str(proxy_port))
            command = ["bash", str(check.RECORDINGS / "record_dynamo_messages_cassettes.sh")]
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=45)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            files = list(output.glob("*.yaml"))
            self.assertEqual(len(files), 2)
            before = {p: p.read_bytes() for p in files}
            for path in files:
                check.validate(path, "qwen3")
            fail[0] = True
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=45)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(before, {p: p.read_bytes() for p in files})
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    unittest.main()
