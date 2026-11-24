#!/usr/bin/env python3
"""Verify the reasoning effort Codex requests for a model-specific catalog.

The launcher writes the levels the gateway advertises into the isolated Codex
model catalog, and Codex starts on `default_reasoning_level` when the user selects
no effort. These runs replay the committed Responses reasoning cassette through
the pinned Codex client and assert the `reasoning.effort` the upstream request
actually carries: `low` for a model advertised as `["low", "high", "max"]`, `max`
when that level is selected explicitly, and `medium` for a model that keeps the
default levels.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import claude_code_replay_server as replay

# The image scenario owns the shared launcher, readiness, and process helpers.
from codex_image_smoke import choose_port, run_launcher, stop, wait_ready


ROOT = Path(__file__).resolve().parent.parent
CASSETTE = (
    ROOT / "crates/agentic-server-core/tests/cassettes/reasoning/responses"
    "/reasoning-single-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"
)
MODEL = "Qwen/Qwen3-30B-A3B-FP8"
PROMPT = "Reply with exactly one word: HELLO"
# The configured set omits `medium`, so Codex starts on the first advertised level.
CONFIGURED_LEVELS = ["low", "high", "max"]
# (label, whether the served model is configured with explicit levels, extra Codex arguments, expected effort)
CASES = (
    ("configured-default", True, [], "low"),
    ("configured-explicit-max", True, ["-c", "model_reasoning_effort=max"], "max"),
    ("unconfigured-default", False, [], "medium"),
)


def binary_path(variable: str, default: str) -> str:
    """Resolve an overriding environment path, or the default relative to the repository root."""
    configured = os.environ.get(variable)
    return str(Path(configured).resolve()) if configured else str(ROOT / default)


def model_config(configure_levels: bool) -> str:
    # Every case configures the served model, so the level override is the only difference.
    lines = [f"[models.{json.dumps(MODEL)}]", 'input_modalities = ["text"]']
    if configure_levels:
        lines.append(f"supported_reasoning_levels = {json.dumps(CONFIGURED_LEVELS)}")
    return "\n".join(lines) + "\n"


def upstream_effort(records: list[dict[str, Any]]) -> object:
    """The reasoning effort on the single captured Responses request."""
    responses = [record["body"] for record in records if record["kind"] == "responses"]
    assert len(responses) == 1, f"expected one Responses request, got {len(responses)}"
    return responses[0].get("reasoning", {}).get("effort")


def run_case(label: str, configure_levels: bool, codex_arguments: list[str], expected_effort: str) -> None:
    with tempfile.TemporaryDirectory(prefix=f"agentic-codex-reasoning-{label}-") as temporary:
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
            (root / "config.toml").write_text(model_config(configure_levels))
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
            with (root / "gateway.log").open("w") as gateway_log:
                gateway = subprocess.Popen(
                    [binary_path("AGENTIC_SERVER_BIN", "target/debug/agentic-server"),
                     "--llm-api-base", upstream, "--gateway-host", "127.0.0.1",
                     "--gateway-port", str(gateway_port), "--skip-llm-ready-check"],
                    env=environment, stdout=gateway_log, stderr=subprocess.STDOUT,
                )
                wait_ready(gateway_url + "/ready", gateway)
                launch = [binary_path("AGENTIC_BIN", "target/debug/agentic"), "harness", "codex",
                          "--gateway-url", gateway_url, "--model", MODEL, "--quiet"]
                result = run_launcher(
                    [*launch, "--", *codex_arguments, "exec", "--skip-git-repo-check", PROMPT], environment,
                )
            assert result.returncode == 0, f"{label}: launcher failed:\n{result.stdout}\n{result.stderr}"
            assert result.stdout.strip() == "HELLO", f"{label}: missing recorded answer: {result.stdout!r}"
            records = replay.load_capture(capture)
            replay.validate_responses_capture(records, MODEL, expected_reasoning_effort=expected_effort)
            print(f"Codex reasoning {label}: upstream reasoning.effort={upstream_effort(records)}")
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
    for label, configure_levels, codex_arguments, expected_effort in CASES:
        run_case(label, configure_levels, codex_arguments, expected_effort)


if __name__ == "__main__":
    main()
