"""Fake client-tool implementations for max_tool_calls cassette recording.

Loaded by record_cassette.py's --tool-outputs. The function name matches the
client-owned function declared in tools-web-search-client.json.
"""


def get_weather(city: str):
    return {"city": city, "temperature_c": 22, "condition": "Clear"}
