"""Bounded, immutable checkpoints for stateless Responses cassette recording.

This is recorder-only wire capture, not a gateway ingestion or projection path.
Output items are replayed verbatim, including opaque reasoning and item order.
"""

import json

MAX_TURNS = 64
MAX_HISTORY_ITEMS = 4096
MAX_HISTORY_BYTES = 4 * 1024 * 1024
MAX_CHECKPOINT_BYTES = 32 * 1024 * 1024


def encode_history(items: list[dict]) -> bytes:
    if len(items) > MAX_HISTORY_ITEMS:
        raise ValueError("manual replay history exceeds the item limit")
    if not all(isinstance(item, dict) for item in items):
        raise ValueError("manual replay history must contain only item objects")
    encoded = bytearray()
    encoder = json.JSONEncoder(ensure_ascii=False, separators=(",", ":"), allow_nan=False)
    for part in encoder.iterencode(items):
        chunk = part.encode("utf-8")
        if len(chunk) > MAX_HISTORY_BYTES - len(encoded):
            raise ValueError("manual replay history exceeds the byte limit")
        encoded.extend(chunk)
    return bytes(encoded)


class ReplayHistory:
    """Keep explicit, byte-bounded checkpoints; branches never inherit siblings."""

    def __init__(self) -> None:
        self._checkpoints: dict[int, bytes] = {}
        self._bytes = 0

    def request_input(self, parent: int | None, new_input: str | list) -> list[dict]:
        if parent is None:
            items = []
        elif parent in self._checkpoints:
            items = json.loads(self._checkpoints[parent])
        else:
            raise ValueError("manual replay branch has no completed checkpoint")
        if isinstance(new_input, str):
            items.append({"type": "message", "role": "user", "content": new_input})
        elif isinstance(new_input, list):
            items.extend(new_input)
        else:
            raise ValueError("manual replay input must be a string or item array")
        # Validate the wire budget and detach the request from caller-owned data.
        return json.loads(encode_history(items))

    def record(self, turn: int, request_input: list[dict], response: dict | None) -> None:
        if turn in self._checkpoints or len(self._checkpoints) >= MAX_TURNS:
            raise ValueError("manual replay checkpoint is duplicate or exceeds the turn limit")
        if not isinstance(response, dict) or response.get("status") != "completed":
            raise ValueError("manual replay requires an explicitly completed response")
        if not isinstance(response.get("id"), str) or not response["id"]:
            raise ValueError("manual replay requires a nonempty response ID")
        output = response.get("output")
        if not isinstance(output, list):
            raise ValueError("manual replay requires a response output array")
        checkpoint = encode_history([*request_input, *output])
        if len(checkpoint) > MAX_CHECKPOINT_BYTES - self._bytes:
            raise ValueError("manual replay checkpoints exceed the total byte limit")
        self._checkpoints[turn] = checkpoint
        self._bytes += len(checkpoint)


class CompletedOutput:
    """Select recorded item completions verbatim, without folding any deltas.

    Some providers return different opaque bytes in the terminal envelope. Qualification
    must explicitly exercise the item-completion bytes retained by the gateway.
    This recorder-only selector does not validate the gateway's semantic lifecycle;
    the Rust replay test still runs the one production ingestion state machine.
    """

    def __init__(self) -> None:
        self._items: dict[int, bytes] = {}
        self._bytes = 0

    def observe(self, event: dict) -> None:
        if not isinstance(event, dict):
            raise ValueError("recorded streaming event must be an object")
        if event.get("type") != "response.output_item.done":
            return
        index = event.get("output_index")
        if type(index) is not int or not 0 <= index < MAX_HISTORY_ITEMS or index in self._items:
            raise ValueError("recorded item completion has an invalid or duplicate index")
        encoded = encode_history([event.get("item")])
        if len(encoded) > MAX_HISTORY_BYTES - self._bytes:
            raise ValueError("recorded item completions exceed the byte limit")
        self._items[index] = encoded
        self._bytes += len(encoded)

    def replay(self, response: dict | None) -> dict:
        if not isinstance(response, dict) or response.get("status") != "completed":
            raise ValueError("item-completion replay requires a completed terminal response")
        terminal = response.get("output")
        if not isinstance(terminal, list) or len(terminal) > MAX_HISTORY_ITEMS or not all(
            isinstance(item, dict) for item in terminal
        ) or set(self._items) != set(range(len(terminal))):
            raise ValueError("recorded item completions do not cover terminal output")
        items = [json.loads(self._items[index])[0] for index in range(len(terminal))]
        if any(item.get("id") != final.get("id") or item.get("type") != final.get("type")
               for item, final in zip(items, terminal)):
            raise ValueError("recorded item completion identity differs from terminal output")
        return {**response, "output": items}
