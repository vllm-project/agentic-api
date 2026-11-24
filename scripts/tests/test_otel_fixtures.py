"""Tests for the telemetry smoke checker in otel_fixtures.py."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import otel_fixtures  # noqa: E402

SERVICE = "agentic-test"


def resource(service: str) -> list[str]:
    return [
        "info\tResourceMetrics #0",
        "Resource attributes:",
        f"     -> service.name: Str({service})",
        "ScopeMetrics #0",
        "InstrumentationScope agentic_core ",
    ]


def metric(name: str, attributes: dict[str, str] | None = None) -> list[str]:
    lines = ["Metric #0", "Descriptor:", f"     -> Name: {name}", "     -> DataType: Sum", "NumberDataPoints #0"]
    if attributes:
        lines.append("Data point attributes:")
        lines += [f"     -> {key}: Str({value})" for key, value in attributes.items()]
    return [*lines, "StartTimestamp: 2026-01-01 00:00:00 +0000 UTC", "Value: 1"]


def complete_log(extra: list[str] | None = None) -> str:
    """One point for every allow-listed instrument, as the Collector prints them."""
    instruments, _ = otel_fixtures.load_allow_list()
    lines = resource(SERVICE)
    for name in instruments:
        lines += metric(name)
    lines += extra or []
    return "\n".join(f"otel-collector-1  | {line}" for line in lines)


PROMETHEUS = f'agentic_execution_count_total{{agentic_api="responses",service_name="{SERVICE}"}} 1\n'


def run_check(log: str, prometheus: str = PROMETHEUS) -> None:
    with tempfile.TemporaryDirectory() as directory:
        log_path, metrics_path = Path(directory, "collector.log"), Path(directory, "metrics.txt")
        log_path.write_text(log)
        metrics_path.write_text(prometheus)
        otel_fixtures.check(SERVICE, str(log_path), str(metrics_path))


class AllowListTest(unittest.TestCase):
    def test_reads_the_rust_harness_tables(self) -> None:
        instruments, values = otel_fixtures.load_allow_list()
        self.assertIn("agentic.execution.count", instruments)
        self.assertEqual(instruments["agentic.websocket.connections.active"], [])
        self.assertEqual(values["agentic.api"], ["responses", "messages"])
        self.assertIn("cancelled", values["error.type"])


class ParseTest(unittest.TestCase):
    def test_points_without_attributes_are_still_points(self) -> None:
        log = "\n".join(resource(SERVICE) + metric("agentic.execution.active") + metric("a", {"k": "v"}))
        self.assertEqual(
            otel_fixtures.parse_debug_metrics(log),
            [(SERVICE, "agentic.execution.active", {}), (SERVICE, "a", {"k": "v"})],
        )

    def test_points_belong_to_their_resource(self) -> None:
        log = "\n".join(resource("other") + metric("a") + resource(SERVICE) + metric("b"))
        self.assertEqual(
            [(service, name) for service, name, _ in otel_fixtures.parse_debug_metrics(log)],
            [("other", "a"), (SERVICE, "b")],
        )


class CheckTest(unittest.TestCase):
    def test_accepts_every_instrument_with_allowed_attributes(self) -> None:
        run_check(complete_log(metric("agentic.execution.count", {"agentic.api": "responses"})))

    def test_rejects_an_unlisted_attribute_key(self) -> None:
        with self.assertRaisesRegex(SystemExit, "unlisted attribute response.id"):
            run_check(complete_log(metric("agentic.execution.count", {"response.id": "resp_1"})))

    def test_rejects_an_unlisted_attribute_value(self) -> None:
        with self.assertRaisesRegex(SystemExit, "outside the allowed values"):
            run_check(complete_log(metric("agentic.execution.count", {"agentic.api": "gpt-4o"})))

    def test_rejects_an_unlisted_instrument(self) -> None:
        with self.assertRaisesRegex(SystemExit, "unlisted instrument agentic.surprise"):
            run_check(complete_log(metric("agentic.surprise")))

    def test_rejects_missing_instruments(self) -> None:
        log = "\n".join(resource(SERVICE) + metric("agentic.execution.count"))
        with self.assertRaisesRegex(SystemExit, "instruments missing"):
            run_check(log)

    def test_ignores_other_services(self) -> None:
        log = complete_log() + "\n" + "\n".join(resource("other") + metric("agentic.surprise"))
        run_check(log)

    def test_requires_the_prometheus_series(self) -> None:
        with self.assertRaisesRegex(SystemExit, "Prometheus endpoint"):
            run_check(complete_log(), prometheus="")


if __name__ == "__main__":
    unittest.main()
