#!/usr/bin/env python3
"""Exercise actual Codex image attachments through both Agentic API launchers.

Responses are replayed from the committed gateway/vLLM vision recording. These
checks verify transport and catalog propagation, not fresh model understanding.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import signal
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer

import claude_code_replay_server as replay


ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "crates/agentic-server-core/tests/cassettes/images"
IMAGE = FIXTURES / "inputs/red-blue-64.png"
MODEL = "Qwen/Qwen2.5-VL-3B-Instruct"
CASSETTE = FIXTURES / "responses/image-single-image-gateway-Qwen-Qwen2.5-VL-3B-Instruct-streaming.yaml"
PROMPT = "Reply with exactly two words: the color on the left half of this image, then the color on the right half."


def choose_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_ready(url: str, process: subprocess.Popen) -> None:
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        assert process.poll() is None, "gateway exited before becoming ready"
        try:
            with opener.open(url, timeout=1) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(0.1)
    raise AssertionError("gateway did not become ready within 30 seconds")


def stop(process: subprocess.Popen | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def run_launcher(command: list[str], environment: dict[str, str], timeout: float = 60) -> subprocess.CompletedProcess:
    # A separate process group lets a timeout stop the integrated gateway and
    # Codex as well as their launcher; SIGKILL alone bypasses Rust destructors.
    process = subprocess.Popen(
        command, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, start_new_session=os.name == "posix",
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    except BaseException:
        if os.name == "posix":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=5)
        else:
            stop(process)
        raise


def run_case(mode: str, supports_image: bool) -> None:
    label = f"{mode}-{'image' if supports_image else 'text-control'}"
    with tempfile.TemporaryDirectory(prefix=f"agentic-codex-{label}-") as temporary:
        root = Path(temporary)
        capture = root / "capture.jsonl"
        capture.write_text("")
        state = replay.ReplayState(replay.load_turns(CASSETTE), capture, model=MODEL)
        server = ThreadingHTTPServer(("127.0.0.1", 0), replay.make_handler(state))
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        gateway = None
        try:
            gateway_port = choose_port()
            upstream = f"http://127.0.0.1:{server.server_port}"
            gateway_url = f"http://127.0.0.1:{gateway_port}"
            modalities = ["text", "image"] if supports_image else ["text"]
            (root / "config.toml").write_text(
                f"[models.{json.dumps(MODEL)}]\ninput_modalities = {json.dumps(modalities)}\n"
            )
            environment = {
                **os.environ,
                "AGENTIC_API_HOME": str(root),
                "AGENTIC_CODEX_BIN": os.environ.get("CODEX_BIN", "codex"),
                "DATABASE_URL": f"sqlite://{root / 'agentic.db'}",
                "RUST_LOG": "warn",
                "OPENAI_API_KEY": "must-not-be-forwarded",
            }
            # The launcher must probe the actual pinned client used by this test.
            environment.pop("AGENTIC_CODEX_CLIENT_VERSION", None)
            agentic = str(Path(os.environ.get("AGENTIC_BIN", "target/debug/agentic")).resolve())
            with (root / "gateway.log").open("w") as gateway_log:
                if mode == "harness":
                    gateway_binary = str(Path(os.environ.get("AGENTIC_SERVER_BIN", "target/debug/agentic-server")).resolve())
                    gateway = subprocess.Popen(
                        [gateway_binary, "--llm-api-base", upstream, "--gateway-host", "127.0.0.1",
                         "--gateway-port", str(gateway_port), "--skip-llm-ready-check"],
                        env=environment, stdout=gateway_log, stderr=subprocess.STDOUT,
                    )
                    wait_ready(gateway_url + "/ready", gateway)
                    launch = [agentic, "harness", "codex", "--gateway-url", gateway_url, "--model", MODEL, "--quiet"]
                else:
                    launch = [agentic, "run", "codex", "--upstream", upstream, "--model", MODEL,
                              "--gateway-host", "127.0.0.1", "--gateway-port", str(gateway_port),
                              "--skip-llm-ready-check", "--quiet"]
                result = run_launcher(
                    [*launch, "--", "exec", "--skip-git-repo-check", "--image", str(IMAGE), "--", PROMPT],
                    environment,
                )
            assert result.returncode == 0, f"{label}: launcher failed:\n{result.stdout}\n{result.stderr}"
            assert "Red, Blue" in result.stdout, f"{label}: missing recorded answer: {result.stdout!r}"
            replay.validate_responses_capture(
                replay.load_capture(capture), MODEL, expected_images=[IMAGE] if supports_image else [],
            )
            print(f"Codex {label}: recorded answer and exact upstream image capture verified")
        except Exception:
            for name in ("gateway.log", "capture.jsonl"):
                path = root / name
                if path.exists():
                    print(f"{label} {name}:\n{path.read_text()}")
            raise
        finally:
            stop(gateway)
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive(), "replay server failed to stop"


def main() -> None:
    for mode, images in [("harness", True), ("run", True), ("harness", False)]:
        run_case(mode, images)


if __name__ == "__main__":
    main()
