from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True, slots=True)
class SSEFrame:
    event: str | None
    data: str


def parse_sse_frames(text: str) -> list[SSEFrame]:
    """
    Parse an SSE body into frames.

    We only care about `event:` and `data:` lines.
    - Multiple `data:` lines are joined with a newline.
    - Blank line terminates a frame.
    """
    frames: list[SSEFrame] = []
    current_event: str | None = None
    data_lines: list[str] = []

    for raw_line in text.splitlines():
        line = raw_line.rstrip("\r")
        if line == "":
            if current_event is not None or data_lines:
                frames.append(SSEFrame(event=current_event, data="\n".join(data_lines)))
            current_event = None
            data_lines = []
            continue

        if line.startswith("event:"):
            current_event = line[len("event:") :].lstrip()
            continue
        if line.startswith("data:"):
            data_lines.append(line[len("data:") :].lstrip())
            continue

    if current_event is not None or data_lines:
        frames.append(SSEFrame(event=current_event, data="\n".join(data_lines)))

    return frames


def sse_has_done_marker(frames: list[SSEFrame]) -> bool:
    return any(f.data == "[DONE]" for f in frames)


def parse_sse_json_events(frames: list[SSEFrame]) -> list[dict[str, Any]]:
    """
    Parse JSON payloads from SSE frames, excluding the terminal `[DONE]` marker.
    """
    events: list[dict[str, Any]] = []
    for frame in frames:
        if frame.data == "[DONE]" or frame.data == "":
            continue
        try:
            payload = json.loads(frame.data)
        except json.JSONDecodeError as e:
            raise AssertionError(f"invalid SSE JSON payload: {frame.data!r}") from e
        if not isinstance(payload, dict):
            raise AssertionError(f"SSE JSON payload is not an object: {type(payload)!r}")
        events.append(payload)
    return events


def index_of_event_type(events: list[dict[str, Any]], event_type: str) -> int:
    for i, event in enumerate(events):
        if event.get("type") == event_type:
            return i
    raise AssertionError(f"event type not found: {event_type!r}")


def extract_completed_response(events: list[dict[str, Any]]) -> dict[str, Any]:
    for event in events:
        if event.get("type") == "response.completed":
            resp = event.get("response")
            if not isinstance(resp, dict):
                raise AssertionError(
                    "response.completed payload did not include a response object"
                )
            return resp
    raise AssertionError("response.completed not found in SSE stream")
