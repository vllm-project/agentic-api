#!/usr/bin/env python3
"""
HTTP Proxy with Request/Response Recording
Records all traffic to YAML files for later analysis.

```bash
python scripts/recorder_proxy.py
```
"""

import argparse
import json
from contextlib import asynccontextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, AsyncGenerator

import uvicorn
from fastapi import FastAPI, Request, Response
from fastapi.responses import ORJSONResponse, StreamingResponse
from httpx import AsyncClient, HTTPStatusError
from loguru import logger
from pydantic_ai.retries import AsyncTenacityTransport, RetryConfig, wait_retry_after
from tenacity import retry_if_exception_type, stop_after_attempt, wait_exponential
from uuid_utils import uuid7
from yaml import dump as yaml_dump

# Defaults
OUTPUT_DIR = Path("responses/tests/cassettes")
TARGET_HOST = "https://api.openai.com"
TIMEOUT = 60 * 5  # 5 minutes


def should_retry_status(response) -> None:
    """Raise exceptions for retryable HTTP status codes."""
    if response.status_code in (429, 502, 503, 504):
        response.raise_for_status()  # This will raise HTTPStatusError


HTTP_CLIENT = AsyncClient(
    timeout=TIMEOUT,
    transport=AsyncTenacityTransport(
        config=RetryConfig(
            # Retry on HTTP errors and connection issues
            retry=retry_if_exception_type((HTTPStatusError, ConnectionError)),
            # Smart waiting: respects Retry-After headers, falls back to exponential backoff
            wait=wait_retry_after(
                fallback_strategy=wait_exponential(multiplier=1, max=60),
                max_wait=300,
            ),
            # Stop after 5 attempts
            stop=stop_after_attempt(5),
            # Re-raise the last exception if all retries fail
            reraise=True,
        ),
        validate_response=should_retry_status,
    ),
)

# Headers to record (whitelist)
RECORDED_HEADERS = {
    "content-type",
    "content-length",
    "authorization",
    "user-agent",
    "accept",
    "accept-encoding",
    "cache-control",
    "connection",
    # Helpful for grouping multi-request tool loops into a single "run"
    "x-run-id",
}

# Headers to exclude when forwarding response (they're handled by httpx/client)
EXCLUDED_RESPONSE_HEADERS = {
    "content-encoding",  # Already decoded by httpx
    "content-length",  # Will be recalculated
    "transfer-encoding",  # Handled by the server
    "connection",  # Let the server handle this
}


def mask_authorization(value: str) -> str:
    """
    Mask Authorization header values in recordings.

    Cassettes are intended to be shareable "golden" artifacts; never write any
    API key material to disk (even partially).
    """
    if not value:
        return value
    lower = value.lower()
    if lower.startswith("bearer "):
        return "Bearer ***"
    return "***"


def filter_headers(headers) -> dict:
    """Filter headers to only include whitelisted ones."""
    return {
        k: v if k.lower() != "authorization" else mask_authorization(v)
        for k, v in headers.items()
        if k.lower() in RECORDED_HEADERS
    }


def filter_response_headers(headers) -> dict:
    """Filter response headers to exclude problematic ones."""
    return {k: v for k, v in headers.items() if k.lower() not in EXCLUDED_RESPONSE_HEADERS}


def save_recording(recording: dict[str, Any], filename: str):
    """Save recording to file."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    filepath = OUTPUT_DIR / filename
    # with open(filepath, "w", encoding="utf-8") as f:
    #     json.dump(recording, f, indent=2, ensure_ascii=False)
    with open(filepath.with_suffix(".yaml"), "w", encoding="utf-8") as f:
        yaml_dump(recording, f, allow_unicode=True, encoding="utf-8")


@asynccontextmanager
async def lifespan(app: FastAPI):
    yield
    await HTTP_CLIENT.aclose()


app = FastAPI(title="Recording Proxy", lifespan=lifespan)


@app.api_route(
    "/{path:path}",
    methods=["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"],
)
async def proxy_request(request: Request, path: str) -> Response:
    """Proxy all requests and record them."""
    # Generate request ID
    request_id = str(uuid7())
    now = datetime.now()
    filename = f"{now.strftime('%Y-%m-%d')}-{request_id}"
    logger.info(f"Recording `{path}` to `{OUTPUT_DIR / filename}`...")

    # Build target URL
    query_string = str(request.query_params)
    target_url = f"{TARGET_HOST}/{path}"
    if query_string:
        target_url += f"?{query_string}"

    # Parse request body
    body = await request.body()
    if not body:
        body = None
    parsed_body = json.loads(body.decode("utf-8"))

    # Prepare recording structure
    recording: dict[str, Any] = {
        "timestamp": now.astimezone(timezone.utc).isoformat(),
        "request_id": request_id,
        "filename": filename,
        "request": {
            "method": request.method,
            "path": f"/{path}",
            "query_params": dict(request.query_params),
            "headers": filter_headers(request.headers),
            "body": parsed_body,
        },
        "response": {
            "status_code": None,
            "headers": {},
        },
    }

    # Make the request
    new_headers = {k: v for k, v in request.headers.items() if k.lower() != "host"}
    media_type = "application/json"
    if parsed_body.get("stream", False):

        async def _stream() -> AsyncGenerator[Response | str, None]:
            async with HTTP_CLIENT.stream(
                method=request.method,
                url=target_url,
                headers=new_headers,
                content=body,
                timeout=TIMEOUT,
            ) as response:
                yield response

                if response.status_code != 200:
                    chunk_str = (await response.aread()).decode()
                    if media_type == "application/json":
                        recording["response"]["body"] = json.loads(chunk_str)
                    else:
                        recording["response"]["body"] = chunk_str
                    yield chunk_str
                else:
                    sse_events = []
                    try:
                        async for chunk_str in response.aiter_lines():
                            chunk_str = f"{chunk_str}\n"
                            # Immediately yield the chunk
                            yield chunk_str
                            sse_events.append(chunk_str)
                    except Exception as e:
                        # Preserve partial SSE in the cassette even if the stream terminates unexpectedly.
                        recording["response"]["stream_error"] = f"{e.__class__.__name__}: {e}"
                    finally:
                        recording["response"]["sse"] = sse_events
                recording["response"]["status_code"] = response.status_code
                recording["response"]["headers"] = filter_headers(response.headers)
                save_recording(recording, recording["filename"])
                logger.info("Streaming completed.")

        agen = _stream()
        response: Response = await anext(agen)
        media_type = response.headers.get("content-type", "text/event-stream")
        return StreamingResponse(
            agen,
            status_code=response.status_code,
            headers=filter_response_headers(response.headers),
            media_type=media_type,
        )
    # Regular request
    else:
        response = await HTTP_CLIENT.request(
            method=request.method,
            url=target_url,
            headers=new_headers,
            content=body,
            timeout=TIMEOUT,
        )
        media_type = response.headers.get("content-type", "application/json")
        if response.status_code != 200:
            body = response.text
            if media_type == "application/json":
                body = json.loads(body)
        else:
            body = response.json()
        recording["response"]["body"] = body
        recording["response"]["status_code"] = response.status_code
        recording["response"]["headers"] = filter_headers(response.headers)
        save_recording(recording, recording["filename"])
        return ORJSONResponse(
            content=body,
            status_code=response.status_code,
            headers=filter_response_headers(response.headers),
            media_type=media_type,
        )


def main():
    """Run the recording proxy server."""
    global OUTPUT_DIR, TARGET_HOST

    parser = argparse.ArgumentParser(
        description="HTTP Proxy with Request/Response Recording",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--output-dir",
        "-o",
        default=OUTPUT_DIR,
    )
    parser.add_argument(
        "--target",
        "-t",
        default="https://api.openai.com",
    )
    parser.add_argument(
        "--port",
        "-p",
        type=int,
        default=8080,
    )
    parser.add_argument(
        "--host",
        default="0.0.0.0",
    )

    args = parser.parse_args()

    OUTPUT_DIR = Path(args.output_dir)
    TARGET_HOST = args.target.rstrip("/")

    print("🎬 Recording Proxy Starting...")
    print(f"   Proxy: http://{args.host}:{args.port}")
    print(f"   Target: {TARGET_HOST}")
    print(f"   Output: {OUTPUT_DIR.absolute()}")
    print("   Press CTRL+C to stop\n")

    uvicorn.run(
        app,
        host=args.host,
        port=args.port,
        log_level="info",
    )


if __name__ == "__main__":
    main()
