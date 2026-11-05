#!/usr/bin/env python3
"""Exercise Files and vector-store routes through the official OpenAI client."""

from __future__ import annotations

from io import BytesIO
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

from openai import OpenAI


REPO = Path(__file__).resolve().parents[2]
SERVER = Path(os.environ.get("AGENTIC_SERVER_BIN", REPO / "target/debug/agentic-server"))


def unused_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_for_gateway(process: subprocess.Popen[bytes], url: str, log: Path) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(f"gateway exited before startup:\n{log.read_text()}")
        try:
            with urllib.request.urlopen(f"{url}/health", timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise AssertionError(f"gateway did not become ready:\n{log.read_text()}")


def run() -> None:
    if not SERVER.is_file():
        raise AssertionError(f"build the gateway first: cargo build -p agentic-server --bin agentic-server ({SERVER})")

    with tempfile.TemporaryDirectory(prefix="agentic-openai-file-search-") as directory:
        root = Path(directory)
        database = root / "gateway.db"
        database.touch()
        port = unused_port()
        url = f"http://127.0.0.1:{port}"
        log = root / "gateway.log"
        environment = os.environ.copy()
        environment.update(
            {
                "AGENTIC_API_HOME": str(root / "home"),
                "AGENTIC_FILES_STORAGE_DIR": str(root / "files"),
            }
        )
        command = [
            str(SERVER),
            "--llm-api-base", "http://127.0.0.1:1",
            "--gateway-host", "127.0.0.1",
            "--gateway-port", str(port),
            "--db-url", f"sqlite://{database}",
            "--skip-llm-ready-check",
        ]
        with log.open("wb") as output:
            process = subprocess.Popen(command, env=environment, stdout=output, stderr=subprocess.STDOUT)
            try:
                wait_for_gateway(process, url, log)
                with OpenAI(base_url=f"{url}/v1", api_key="integration-test", max_retries=0) as client:
                    content = b"The northern office opens at seven. The southern office opens at nine."
                    upload = BytesIO(content)
                    upload.name = "offices.txt"
                    file = client.files.create(file=upload, purpose="assistants")
                    assert file.filename == "offices.txt"
                    assert client.files.retrieve(file.id).id == file.id
                    assert any(item.id == file.id for item in client.files.list().data)
                    assert client.files.content(file.id).read() == content

                    store = client.vector_stores.create(name="offices")
                    assert client.vector_stores.retrieve(store.id).id == store.id
                    attached = client.vector_stores.files.create(
                        store.id, file_id=file.id, attributes={"region": "north"}
                    )
                    assert attached.status == "completed"
                    assert client.vector_stores.files.retrieve(file.id, vector_store_id=store.id).id == file.id
                    assert any(item.id == file.id for item in client.vector_stores.files.list(store.id).data)

                    matches = client.vector_stores.search(
                        store.id,
                        query="northern office",
                        max_num_results=5,
                        filters={"type": "eq", "key": "region", "value": "north"},
                    )
                    assert any(
                        match.file_id == file.id and any("seven" in part.text for part in match.content)
                        for match in matches.data
                    ), matches

                    assert client.vector_stores.files.delete(file.id, vector_store_id=store.id).deleted
                    assert client.files.retrieve(file.id).id == file.id
                    assert client.vector_stores.delete(store.id).deleted
                    assert client.files.delete(file.id).deleted
            except Exception as error:
                raise AssertionError(f"OpenAI client integration failed: {error}\ngateway log:\n{log.read_text()}") from error
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":
    run()
