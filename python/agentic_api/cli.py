from __future__ import annotations

import argparse
import math
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence
from urllib.parse import urlparse

from agentic_api import __version__
from agentic_api.binary import find_packaged_binary
from agentic_api.diagnostics import doctor
from agentic_api.version import version_report


DEFAULT_HOST = "0.0.0.0"
DEFAULT_PORT = 9000
DEFAULT_STARTUP_TIMEOUT_S = 600.0
DEFAULT_SHUTDOWN_TIMEOUT_S = 10.0
DEFAULT_VLLM_PORT = 8000
DEFAULT_GATEWAY_API_KEY_ENV = "OPENAI_API_KEY"
DEFAULT_VLLM_API_KEY_ENV = "AGENTIC_VLLM_API_KEY"
RESERVED_VLLM_FLAGS = {"--host", "--port", "--api-key"}
INCOMPATIBLE_VLLM_FLAGS = {"--uds"}
MAX_TIMEOUT_S = 86_400.0


@dataclass(frozen=True)
class ServeOptions:
    mode: str
    model: str | None
    vllm_base_url: str | None
    host: str
    port: int
    startup_timeout_s: float
    shutdown_timeout_s: float
    vllm_port: int
    gateway_api_key_env: str
    vllm_api_key_env: str
    vllm_args: list[str]


class _AgenticArgumentParser(argparse.ArgumentParser):
    def parse_args(self, args: Sequence[str] | None = None, namespace: argparse.Namespace | None = None) -> argparse.Namespace:
        parsed = super().parse_args(args=args, namespace=namespace)
        command = getattr(parsed, "command", None)

        if command == "serve":
            parsed.options = _build_serve_options(self, parsed)

        return parsed


def build_parser() -> argparse.ArgumentParser:
    parser = _AgenticArgumentParser(
        prog="agentic-api",
        description="Python launcher for packaged Agentic API binaries",
        allow_abbrev=False,
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser(
        "run", help="Run the gateway: direct inference by default, HTTP with --upstream",
        description="Uses the Rust launcher; run agentic-api run --help for its options.",
        add_help=False,
    )

    serve_parser = subparsers.add_parser(
        "serve",
        help="Launch Agentic API in local or remote mode",
        description="Launch the packaged Rust gateway. Choose exactly one of --model (managed local vLLM) "
        "or --vllm-base-url (an existing upstream server).",
        epilog="Pass extra vLLM arguments after -- in local mode. The launcher reserves --host, --port, "
        "and --api-key; --uds is incompatible. Ports must be 1-65535. Timeouts must be greater than 0 "
        "and at most 86400 seconds.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        allow_abbrev=False,
    )
    serve_parser.add_argument("--model", help="Model to serve with managed local vLLM; requires a local vLLM install")
    serve_parser.add_argument("--vllm-base-url", help="HTTP(S) base URL of an existing upstream, without credentials")
    serve_parser.add_argument("--host", default=DEFAULT_HOST, help="Gateway listen address")
    serve_parser.add_argument("--port", type=_tcp_port, default=DEFAULT_PORT, help="Gateway listen port")
    serve_parser.add_argument(
        "--startup-timeout-s", type=_bounded_timeout, default=DEFAULT_STARTUP_TIMEOUT_S,
        help="Startup readiness timeout in seconds",
    )
    serve_parser.add_argument(
        "--shutdown-timeout-s", type=_bounded_timeout, default=DEFAULT_SHUTDOWN_TIMEOUT_S,
        help="Grace period in seconds before terminating child processes forcibly",
    )
    serve_parser.add_argument(
        "--vllm-port", type=_tcp_port, default=DEFAULT_VLLM_PORT, help="Managed local vLLM listen port",
    )
    serve_parser.add_argument(
        "--gateway-api-key-env", default=DEFAULT_GATEWAY_API_KEY_ENV,
        help="Environment variable containing the upstream API key used by the gateway",
    )
    serve_parser.add_argument(
        "--vllm-api-key-env", default=DEFAULT_VLLM_API_KEY_ENV,
        help="Environment variable containing the managed local vLLM API key; also passed to the gateway",
    )
    serve_parser.add_argument("vllm_args", nargs=argparse.REMAINDER, help="Extra local vLLM arguments after --")

    doctor_parser = subparsers.add_parser(
        "doctor",
        help="Report packaged binary and compatibility diagnostics",
        description="Check the packaged gateway and local vLLM compatibility. Without --mode, report both "
        "modes and use remote health for the exit status. Exit 0 when healthy, 1 otherwise.",
        allow_abbrev=False,
    )
    doctor_parser.add_argument("--mode", choices=("local", "remote"), help="Select which mode to check")
    doctor_parser.add_argument(
        "--json", action="store_true", dest="json_output", help="Emit a machine-readable JSON report"
    )

    subparsers.add_parser(
        "version", help="Print package and packaged binary versions",
        description="Print the Python package, packaged Rust binary, and supported and installed vLLM versions. "
        "Use agentic-api --version to print only the Python package version.",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments and arguments[0] == "run":
        # Replace this process so the Rust launcher remains the lifecycle and CLI owner.
        try:
            binary = str(find_packaged_binary("agentic"))
            os.execv(binary, [binary, "serve", *arguments[1:]])
        except OSError as error:
            print(str(error), file=sys.stderr)
            return 1
        return 0
    parser = build_parser()
    try:
        namespace = parser.parse_args(argv)
    except SystemExit as error:
        code = error.code
        return code if isinstance(code, int) else 1

    if namespace.command == "version":
        try:
            print(version_report())
            return 0
        except (FileNotFoundError, RuntimeError) as error:
            print(str(error), file=sys.stderr)
            return 1

    if namespace.command == "doctor":
        return doctor(namespace.mode, json_output=namespace.json_output)

    if namespace.command == "serve":
        try:
            from agentic_api.launcher import run_serve
        except ModuleNotFoundError as error:
            if error.name != "agentic_api.launcher":
                raise
            print(
                "agentic-api serve is unavailable because the Python launcher is missing; "
                "reinstall agentic-api for this platform.",
                file=sys.stderr,
            )
            return 1

        return run_serve(namespace.options)

    parser.error(f"unknown command: {namespace.command}")
    return 1


def _build_serve_options(parser: argparse.ArgumentParser, namespace: argparse.Namespace) -> ServeOptions:
    if namespace.model is not None and not namespace.model.strip():
        parser.error("--model must not be empty")

    source_count = int(bool(namespace.model)) + int(bool(namespace.vllm_base_url))
    if source_count != 1:
        parser.error("exactly one of --model or --vllm-base-url is required")

    vllm_args = _normalize_vllm_args(namespace.vllm_args)
    reserved_flag = _reserved_vllm_flag(vllm_args)
    if reserved_flag is not None:
        parser.error(f"{reserved_flag} is reserved for the launcher and cannot be forwarded after --")

    vllm_base_url = None
    mode = "local"
    if namespace.vllm_base_url is not None:
        vllm_base_url = _normalize_base_url(parser, namespace.vllm_base_url)
        mode = "remote"

    if mode == "remote" and vllm_args:
        parser.error("vLLM passthrough arguments are only supported with --model (local mode)")

    return ServeOptions(
        mode=mode,
        model=namespace.model,
        vllm_base_url=vllm_base_url,
        host=namespace.host,
        port=namespace.port,
        startup_timeout_s=namespace.startup_timeout_s,
        shutdown_timeout_s=namespace.shutdown_timeout_s,
        vllm_port=namespace.vllm_port,
        gateway_api_key_env=namespace.gateway_api_key_env,
        vllm_api_key_env=namespace.vllm_api_key_env,
        vllm_args=vllm_args,
    )


def _normalize_vllm_args(values: Sequence[str]) -> list[str]:
    if values and values[0] == "--":
        return list(values[1:])
    return list(values)


def _reserved_vllm_flag(values: Sequence[str]) -> str | None:
    for value in values:
        if not value.startswith("--"):
            continue
        option_name = value.partition("=")[0].replace("_", "-")
        if len(option_name) > 2 and any(
            reserved.startswith(option_name) for reserved in RESERVED_VLLM_FLAGS | INCOMPATIBLE_VLLM_FLAGS
        ):
            return option_name
    return None


def _tcp_port(value: str) -> int:
    try:
        port = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be between 1 and 65535") from error
    if not 1 <= port <= 65_535:
        raise argparse.ArgumentTypeError("must be between 1 and 65535")
    return port


def _bounded_timeout(value: str) -> float:
    try:
        timeout = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            f"must be finite and greater than 0, up to {MAX_TIMEOUT_S:g} seconds"
        ) from error
    if not math.isfinite(timeout) or not 0 < timeout <= MAX_TIMEOUT_S:
        raise argparse.ArgumentTypeError(f"must be finite and greater than 0, up to {MAX_TIMEOUT_S:g} seconds")
    return timeout


def _normalize_base_url(parser: argparse.ArgumentParser, value: str) -> str:
    if any(character.isspace() for character in value):
        parser.error("--vllm-base-url must not contain whitespace")

    try:
        parsed = urlparse(value)
        hostname = parsed.hostname
        port = parsed.port
    except ValueError as error:
        parser.error(f"--vllm-base-url is malformed: {error}")

    if parsed.scheme not in {"http", "https"} or not parsed.netloc or not hostname:
        parser.error("--vllm-base-url must be an http:// or https:// base URL")
    if parsed.username is not None or parsed.password is not None:
        parser.error("--vllm-base-url must not contain credentials; use an environment variable for API keys")
    if port is not None and not 1 <= port <= 65_535:
        parser.error("--vllm-base-url port must be between 1 and 65535")
    if parsed.query or parsed.fragment:
        parser.error("--vllm-base-url must not include a query string or fragment")
    return value.rstrip("/")
