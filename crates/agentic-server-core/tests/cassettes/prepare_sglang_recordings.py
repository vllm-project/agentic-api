#!/usr/bin/env python3
"""Attach provenance and sanitize recorder-produced SGLang cassettes in staging."""

import json
import sys
from pathlib import Path
from urllib.request import urlopen

import yaml


def prepare(root: Path, version: str, model: str, upstream: str) -> None:
    with urlopen(upstream.rstrip("/") + "/server_info", timeout=15) as response:
        info = json.load(response)
    if info["version"] != version or info["model_path"] != model:
        raise ValueError("Running SGLang version/model does not match the requested recording profile")
    launch = {
        key: info[key]
        for key in (
            "revision", "reasoning_parser", "tool_call_parser", "context_length",
            "mem_fraction_static", "dtype", "quantization", "tp_size", "random_seed",
        )
    }
    for path in sorted(root.glob("*.yaml")):
        cassette = yaml.safe_load(path.read_text())
        identifiers = {}

        def sanitize(value, key=""):
            if isinstance(value, dict):
                return {name: sanitize(item, name) for name, item in value.items()}
            if isinstance(value, list):
                return [sanitize(item, key) for item in value]
            if key in {"created_at", "completed_at"} and isinstance(value, (float, int)):
                # Keep provider wire number types while removing wall-clock time.
                return 0.0 if isinstance(value, float) else 0
            if key in {"id", "item_id", "call_id", "previous_response_id"} and isinstance(value, str):
                if value not in identifiers:
                    identifiers[value] = f"recorded_{len(identifiers) + 1}"
                return identifiers[value]
            return value

        for turn in cassette["turns"]:
            turn["request"]["headers"] = {"content-type": "application/json"}
            turn["request"]["query_params"] = {}
            turn["request"]["body"] = sanitize(turn["request"]["body"])
            response = turn["response"]
            if response.get("body") is not None:
                response["body"] = sanitize(response["body"])
            else:
                sanitized = []
                for raw in response["sse"]:
                    lines = []
                    for line in raw.splitlines(keepends=True):
                        if line.startswith("data:") and line[5:].strip() != "[DONE]":
                            ending = "\n" if line.endswith("\n") else ""
                            line = "data: " + json.dumps(sanitize(json.loads(line[5:])), separators=(",", ":")) + ending
                        lines.append(line)
                    sanitized.append("".join(lines))
                response["sse"] = sanitized
        cassette["provider"] = {
            "name": "sglang",
            "version": version,
            "model": model,
            "transport": "http-sse" if cassette["turns"][0]["request"]["body"]["stream"] else "http-json",
            "launch": launch,
            "capabilities_exercised": (
                ["text", "gateway_stateful_continuation"]
                if "stateful" in path.name else ["client_function_call"]
            ),
            "unverified": ["parallel_function_calls", "structured_text", "upstream_websocket", "reasoning_summary"],
        }
        path.write_text(yaml.safe_dump(cassette, sort_keys=False, allow_unicode=True, width=10**9))


if __name__ == "__main__":
    prepare(Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4])
