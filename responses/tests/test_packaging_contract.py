from __future__ import annotations

import tomllib
from pathlib import Path


def test_console_scripts_expose_supervisor_and_vllm_shim() -> None:
    pyproject = Path(__file__).resolve().parents[1] / "pyproject.toml"
    data = tomllib.loads(pyproject.read_text(encoding="utf-8"))
    scripts = data["project"]["scripts"]

    assert scripts["vllm-responses"] == "vllm_responses.entrypoints.serve:main"
    assert scripts["vllm"] == "vllm_responses.entrypoints.vllm_cli:main"
