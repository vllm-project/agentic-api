"""Run the shared synthetic citation cases through the official OpenAI client."""

from __future__ import annotations

from contextlib import contextmanager
from http.server import ThreadingHTTPServer
import json
from pathlib import Path
import sys
import threading

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts"))
from claude_code_replay_server import ReplayState, ReplayTurn, make_handler


@contextmanager
def citation_upstream(root: Path):
    state = ReplayState([], root / "inference.jsonl", model="test-model")
    with ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state)) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}", state
        finally:
            server.shutdown()
            worker.join(timeout=5)
            assert not worker.is_alive(), "fixture upstream did not stop"


def inference_turn(item: dict, streaming: bool, deltas: list[str]) -> ReplayTurn:
    response = {
        "id": "resp_fixture", "object": "response", "created_at": 0,
        "model": "test-model", "status": "completed", "output": [item],
    }
    if not streaming:
        return ReplayTurn(200, "application/json", json.dumps(response).encode())
    started = {**item, "status": "in_progress"}
    if item["type"] == "message":
        started["content"] = []
    events = [
        {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}},
        {"type": "response.in_progress", "response": {**response, "status": "in_progress", "output": []}},
        {"type": "response.output_item.added", "output_index": 0, "item": started},
    ]
    if item["type"] == "message":
        part = item["content"][0]
        fields = {"output_index": 0, "content_index": 0, "item_id": item["id"]}
        events.append({
            "type": "response.content_part.added", **fields,
            "part": {"type": "output_text", "text": "", "annotations": []},
        })
        assert "".join(deltas) == part["text"]
        events.extend({"type": "response.output_text.delta", **fields, "delta": delta} for delta in deltas)
        events.append({"type": "response.output_text.done", **fields, "text": part["text"]})
        events.append({"type": "response.content_part.done", **fields, "part": part})
    events.extend([
        {"type": "response.output_item.done", "output_index": 0, "item": item},
        {"type": "response.completed", "response": response},
    ])
    body = "".join(f"data: {json.dumps(event)}\n\n" for event in events) + "data: [DONE]\n\n"
    return ReplayTurn(200, "text/event-stream", body.encode())


def render_case(case: dict, ids: dict[str, str]) -> dict:
    text = "".join(case["deltas"])

    def replace_ids(value: str) -> str:
        for original, generated in ids.items():
            value = value.replace(original, generated)
        return value

    rendered = json.loads(replace_ids(json.dumps({**case, "deltas": [text]}, ensure_ascii=False)))
    rendered["deltas"] = list("".join(rendered["deltas"]))
    for annotation in rendered["annotations"] + rendered["expected_annotations"]:
        if annotation["type"] == "file_citation" and annotation["index"] <= len(text):
            annotation["index"] = len(replace_ids(text[:annotation["index"]]))
    return rendered


def create_response(client, streaming: bool, expected: list[dict], **request):
    def project(parts):
        return [
            {"text": part.text,
             "annotations": [annotation.model_dump(exclude_none=True) for annotation in part.annotations]}
            for part in parts
        ]

    events = []
    if streaming:
        with client.responses.stream(model="test-model", **request) as stream:
            events = list(stream)
            response = stream.get_final_response()
        deltas = "".join(event.delta for event in events if event.type == "response.output_text.delta")
        assert deltas == "".join(part["text"] for part in expected), deltas
        parts = [event.part for event in events if event.type == "response.content_part.done"]
        messages = [
            event.item for event in events
            if event.type == "response.output_item.done" and event.item.type == "message"
        ]
        assert len(messages) == 1
        assert project(parts) == expected
        assert project(messages[0].content) == expected
    else:
        response = client.responses.create(model="test-model", **request)
    message = next(item for item in response.output if item.type == "message")
    assert project(message.content) == expected, {
        "actual": project(message.content), "expected": expected,
        "output": [item.model_dump(exclude_none=True) for item in response.output],
    }
    stored = client.responses.retrieve(response.id)
    stored_message = next(item for item in stored.output if item.type == "message")
    assert stored_message.id == message.id
    assert project(stored_message.content) == expected, "stored annotations differ from the client response"
    return response, events


def exercise_citations(client, state: ReplayState) -> None:
    fixtures_dir = REPO / "crates/agentic-server-core/tests/fixtures/file_search"
    native = json.loads((fixtures_dir / "native_annotations.json").read_text())
    expected = [
        {"text": part["text"], "annotations": part["annotations"]}
        for part in native["response"]["output"][0]["content"]
    ]
    for streaming in [False, True]:
        for client_function in [False, True]:
            if streaming:
                body = "".join(f"data: {json.dumps(event)}\n\n" for event in native["events"])
                turn = ReplayTurn(200, "text/event-stream", (body + "data: [DONE]\n\n").encode())
            else:
                turn = ReplayTurn(200, "application/json", json.dumps(native["response"]).encode())
            with state.lock:
                state.turns.append(turn)
            tools = [{"type": "function", "name": "file_search", "parameters": {"type": "object"}}]
            _, events = create_response(
                client, streaming, expected, input="Answer with sources", store=True,
                tools=tools if client_function else [],
            )
            if streaming:
                added = [
                    event.annotation.model_dump(exclude_none=True) for event in events
                    if event.type == "response.output_text.annotation.added"
                ]
                assert added == [annotation for part in expected for annotation in part["annotations"]]

    fixture = json.loads((fixtures_dir / "citations.json").read_text())
    ids = {}
    store = client.vector_stores.create(name="citation-fixtures")
    try:
        for source in fixture["files"]:
            uploaded = client.files.create(
                file=(source["filename"], source["text"].encode(), "text/plain"), purpose="assistants",
            )
            ids[source["file_id"]] = uploaded.id
            client.vector_stores.files.create(store.id, file_id=uploaded.id)
        for streaming in [False, True]:
            for include_results in [False, True]:
                for source_case in fixture["cases"]:
                    case = render_case(source_case, ids)
                    part = {"type": "output_text", "text": "".join(case["deltas"]), "annotations": case["annotations"]}
                    message = {
                        "type": "message", "id": "msg_fixture", "role": "assistant",
                        "status": "completed", "content": [part],
                    }
                    call = {
                        "type": "function_call", "id": "fc_fixture", "call_id": "call_fixture",
                        "name": "file_search", "arguments": json.dumps({"queries": [case["query"]]}),
                        "status": "completed",
                    }
                    with state.lock:
                        state.turns.extend([
                            inference_turn(call, streaming, []),
                            inference_turn(message, streaming, case["deltas"]),
                            inference_turn(message, streaming, case["deltas"]),
                        ])
                    expected = [{"text": part["text"], "annotations": case["expected_annotations"]}]
                    try:
                        response, _ = create_response(
                            client, streaming, expected, input="Explain the climate evidence",
                            store=True, tools=[{"type": "file_search", "vector_store_ids": [store.id]}],
                            include=["file_search_call.results"] if include_results else [],
                        )
                        call = next(item for item in response.output if item.type == "file_search_call")
                        assert call.status == "completed", call
                        assert (call.results is not None) == include_results
                        create_response(
                            client, streaming, expected, input="Explain those sources again",
                            previous_response_id=response.id, store=True,
                        )
                    except Exception as error:
                        captures = [json.loads(line) for line in state.capture_path.read_text().splitlines()]
                        requests = [record["body"] for record in captures if record["kind"] == "responses"]
                        raise AssertionError(
                            f"{case['name']} stream={streaming} include={include_results}: {error}; "
                            f"last inference input={requests[-1]['input']}"
                        ) from error
        assert state.next_turn == len(state.turns), "not every fixture inference turn was consumed"
    finally:
        client.vector_stores.delete(store.id)
        for file_id in ids.values():
            client.files.delete(file_id)
    print(
        f"OpenAI client: {len(fixture['cases'])} citation fixtures passed over JSON/SSE, "
        "with/without results, storage and continuation"
    )
