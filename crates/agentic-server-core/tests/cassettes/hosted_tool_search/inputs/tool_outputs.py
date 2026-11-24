"""Fake client-tool implementations for hosted tool-search cassette recording.

Loaded by record_cassette.py's --tool-outputs. The names match the deferred
functions and namespace members declared in tools-mixed.json.
"""


def get_weather(city: str):
    return {"city": city, "temperature_c": 18, "condition": "Cloudy"}


def get_timezone(city: str):
    return {"city": city, "time_zone": "Europe/Paris"}


def get_exchange_rate(base: str, quote: str):
    return {"base": base, "quote": quote, "rate": 1.08}
