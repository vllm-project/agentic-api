"""Record Messages threads through a real vLLM upstream using the shared proxy.

Each YAML captures upstream inference. Its JSON companion captures the public
requests/responses, including the delta-only continuation across a gateway restart.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

import httpx
import yaml

from record_cassette import _start_proxy, _stop_proxy
from record_messages_tool_choice import free_port, wait_for_gateway

HERE = Path(__file__).resolve().parent
BETA = "message-threads-2026-08-12"
TOKEN = "THREAD_PROOF_91827"


def message(response, stream):
    if not stream:
        return response.json()
    events = [json.loads(line[5:]) for line in response.text.splitlines() if line.startswith("data:")]
    assert events[-1]["type"] == "message_stop", response.text
    result = dict(next(e["message"] for e in events if e["type"] == "message_start"))
    blocks = {}
    partial = {}
    for event in events:
        kind = event["type"]
        if kind == "content_block_start":
            blocks[event["index"]] = dict(event["content_block"])
        elif kind == "content_block_delta":
            index, delta = event["index"], event["delta"]
            if delta["type"] == "text_delta":
                blocks[index]["text"] += delta["text"]
            elif delta["type"] == "input_json_delta":
                partial[index] = partial.get(index, "") + delta["partial_json"]
        elif kind == "message_delta":
            result.update(event["delta"])
    for index, value in partial.items():
        blocks[index]["input"] = json.loads(value)
    result["content"] = [blocks[i] for i in sorted(blocks)]
    return result


def stop(process):
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
    assert process.returncode == 0, process.returncode


def record(args, kind, stream, directory):
    stem = f"messages-thread-{kind}-{args.model.replace('/', '-')}-{'streaming' if stream else 'nonstreaming'}"
    output = directory / f"{stem}.yaml"
    proxy_port = free_port()
    proxy = _start_proxy(output, args.vllm.rstrip('/'), proxy_port)
    transcript = []
    try:
        with tempfile.TemporaryDirectory(prefix="messages-thread-record-") as temp:
            work = Path(temp)
            env = {k: v for k, v in os.environ.items()
                   if not k.startswith(("OIDC_", "DATABASE_", "MESSAGES_GATEWAY_", "YOU_"))
                   and k not in ("OPENAI_API_KEY", "LLM_API_BASE", "DB_URL")}
            env.update(AGENTIC_API_HOME=temp, RUST_LOG="error")
            request = {"model": args.model, "max_tokens": 256, "stream": stream, "temperature": 0,
                       "chat_template_kwargs": {"enable_thinking": False}, "thread": {"type": "create"},
                       "system": "Follow instructions exactly. Keep answers short.",
                       "messages": [{"role": "user", "content": f"The verification token for this conversation is {TOKEN}."}]}
            if kind == "client":
                request.update(tools=[{"name": "client_echo", "description": "Get the verification token.",
                                       "input_schema": {"type": "object", "properties": {}}}],
                               tool_choice={"type": "auto"})
                request["messages"][0]["content"] = (
                    "Call client_echo exactly once to get a verification token. After its result, "
                    "reply with the entire token verbatim and do not call tools again.")
            for turn in range(2):
                # Restart the gateway each turn, preserving only its database.
                port = free_port()
                url = f"http://127.0.0.1:{port}"
                log = work / f"gateway-{turn}.log"
                with log.open("w") as stdout:
                    process = subprocess.Popen([str(args.binary.resolve()), "--llm-api-base",
                        f"http://127.0.0.1:{proxy_port}", "--gateway-host", "127.0.0.1", "--gateway-port", str(port),
                        "--skip-llm-ready-check", "--db-url", args.database_url or f"sqlite://{work / 'state.db'}"],
                        env=env, stdout=stdout, stderr=subprocess.STDOUT)
                    try:
                        with httpx.Client(timeout=300, trust_env=False) as client:
                            wait_for_gateway(client, url, process, log)
                            response = client.post(url + "/v1/messages?beta=true", json=request,
                                headers={"anthropic-beta": BETA, "x-api-key": "thread-recording-owner"})
                            response.raise_for_status()
                            final = message(response, stream)
                            transcript.append({"request": request, "response": response.text if stream else response.json()})
                    finally:
                        stop(process)
                if turn == 0:
                    content = "Write the verification token from my first message. Output only the token."
                    if kind == "client":
                        calls = [b for b in final["content"] if b["type"] == "tool_use"]
                        assert final["stop_reason"] == "tool_use" and len(calls) == 1, final
                        assert calls[0]["name"] == "client_echo", final
                        content = [{"type": "tool_result", "tool_use_id": calls[0]["id"], "content": TOKEN}]
                    else:
                        assert final["stop_reason"] == "end_turn", final
                    request = {k: v for k, v in request.items() if k not in ("system", "tools", "messages", "thread")}
                    request.update(thread={"type": "continue", "previous_message_id": final["id"]},
                                   messages=[{"role": "user", "content": content}])
                else:
                    assert final["stop_reason"] == "end_turn", final
                    assert TOKEN in "".join(b.get("text", "") for b in final["content"]), final
        turns = yaml.safe_load(output.read_text())["turns"]
        assert len(turns) == 2 and all(t["response"]["status_code"] == 200 for t in turns), turns
        assert all("thread" not in t["request"]["body"] for t in turns)
        assert len(turns[1]["request"]["body"]["messages"]) == 3
        (directory / f"{stem}.json").write_text(json.dumps({"turns": transcript}, indent=2, ensure_ascii=False) + "\n")
        print(f"Validated {stem}: real inference, delta-only continuation, gateway restart.")
    finally:
        _stop_proxy(proxy)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--vllm", required=True)
    parser.add_argument("--model", default="Qwen/Qwen3.8-27B-FP8")
    parser.add_argument("--database-url", help="An isolated test database; defaults to a temporary SQLite file")
    parser.add_argument("--output-dir", type=Path, default=HERE / "messages/threads")
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    # Stage the complete set: failed inference never replaces working recordings.
    with tempfile.TemporaryDirectory(prefix="messages-thread-cassettes-") as temp:
        staging = Path(temp)
        for kind in ("text", "client"):
            for stream in (False, True):
                record(args, kind, stream, staging)
        for path in staging.iterdir():
            (args.output_dir / path.name).write_bytes(path.read_bytes())


if __name__ == "__main__":
    main()
