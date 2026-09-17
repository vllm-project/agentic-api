"""Client-executed `view_image` handler for the recorder's --tool-outputs.

Returns structured content parts, so the recorder submits a
`function_call_output` whose `output` is an array carrying the committed
red|blue PNG as an inline `input_image` -- the shape Codex uses when a tool
hands an image back to the model.
"""

import base64
from pathlib import Path

_IMAGE = Path(__file__).with_name("red-blue-64.png").read_bytes()
_IMAGE_URL = "data:image/png;base64," + base64.b64encode(_IMAGE).decode("ascii")


def view_image(path: str = "") -> list[dict]:
    return [
        {"type": "input_text", "text": f"Loaded {path or 'diagram.png'}:"},
        {"type": "input_image", "image_url": _IMAGE_URL, "detail": "low"},
    ]
