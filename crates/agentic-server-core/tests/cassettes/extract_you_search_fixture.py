#!/usr/bin/env python3
"""Extract a sanitized You.com ``GET /v1/search`` response from a recorded cassette.

The ``messages_multiround`` web-search cassettes were recorded while the gateway
forwarded You.com's ``results`` and ``metadata`` objects verbatim into the
``web_search`` tool output (``7783068``, #131). Rebuilding ``{"results", "metadata"}``
from one of those tool outputs therefore reproduces the provider's wire envelope
and every field it actually returns, which is what the typed normalization tests
in ``tests/web_search_tool_test.rs`` replay.

Sanitization is limited to the identifiers that vary per call: ``search_uuid`` is
replaced with a fixed UUID, ``latency`` is rounded, and the result lists are
truncated to keep the fixture readable. Field names, values, and the presence or
absence of sections (for example a response without ``news``) are preserved.

Usage:
    python tests/cassettes/extract_you_search_fixture.py \
        --cassette tests/cassettes/messages_multiround/sequential-web-search-qwen3-nonstreaming.yaml \
        --query "latest stable Rust version number" --web 3 --news 2 \
        --output tests/fixtures/you_search_response.json
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import yaml

FIXED_SEARCH_UUID = "00000000-0000-4000-8000-000000000000"


def iter_tool_outputs(node: Any) -> Iterator[dict[str, Any]]:
    """Yield every recorded ``web_search`` tool output embedded in a cassette."""
    if isinstance(node, dict):
        for value in node.values():
            yield from iter_tool_outputs(value)
    elif isinstance(node, list):
        for value in node:
            yield from iter_tool_outputs(value)
    elif isinstance(node, str) and node.startswith("{"):
        try:
            parsed = json.loads(node)
        except json.JSONDecodeError:
            return
        if isinstance(parsed, dict) and "results" in parsed and "metadata" in parsed:
            yield parsed


def sanitize(output: dict[str, Any], web: int | None, news: int | None) -> dict[str, Any]:
    results = dict(output["results"])
    if "web" in results and web is not None:
        results["web"] = results["web"][:web]
    if "news" in results and news is not None:
        results["news"] = results["news"][:news]
    metadata = dict(output["metadata"])
    metadata["search_uuid"] = FIXED_SEARCH_UUID
    if isinstance(metadata.get("latency"), float):
        metadata["latency"] = round(metadata["latency"], 3)
    return {"results": results, "metadata": metadata}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--cassette", required=True, type=Path)
    parser.add_argument("--query", required=True, help="metadata.query of the recorded response to extract")
    parser.add_argument("--web", type=int, default=None, help="keep at most this many web results")
    parser.add_argument("--news", type=int, default=None, help="keep at most this many news results")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    with args.cassette.open(encoding="utf-8") as handle:
        cassette = yaml.safe_load(handle)

    matches = [
        output for output in iter_tool_outputs(cassette) if output["metadata"].get("query") == args.query
    ]
    if not matches:
        queries = sorted({output["metadata"].get("query", "") for output in iter_tool_outputs(cassette)})
        print(f"no recorded response for query {args.query!r}; available: {queries}", file=sys.stderr)
        return 1

    fixture = sanitize(matches[0], args.web, args.news)
    args.output.write_text(json.dumps(fixture, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    sections = ", ".join(f"{name}={len(items)}" for name, items in fixture["results"].items())
    print(f"wrote {args.output} ({sections})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
