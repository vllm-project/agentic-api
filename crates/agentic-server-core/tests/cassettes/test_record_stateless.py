"""Offline recorder tests. Mock wire data is not a provider qualification cassette."""

import contextlib
import copy
import io
import json
import struct
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import httpx
from click.testing import CliRunner

import record_cassette as recorder
import recorder_history as history


OPAQUE = "private-opaque-state/+=é"


def completed(turn: int) -> dict:
    return {
        "id": f"resp_{turn}", "status": "completed", "output": [
            {"type": "reasoning", "id": f"rs_{turn}", "summary": [], "encrypted_content": OPAQUE},
            {"type": "function_call", "id": f"fc_{turn}", "call_id": f"call_{turn}",
             "name": "lookup", "arguments": "{}", "status": "completed"},
        ],
    }


class StatelessRecorderTests(unittest.TestCase):
    def test_item_completion_source_preserves_its_bytes_not_terminal_ciphertext(self) -> None:
        response = completed(1)
        original = copy.deepcopy(response)
        selected = history.CompletedOutput()
        for index, item in reversed(list(enumerate(response["output"]))):
            selected.observe({"type": "response.output_item.done", "output_index": index, "item": item})
        response["output"][0]["encrypted_content"] = "distinct-terminal-state"
        self.assertEqual(selected.replay(response), original)
        with self.assertRaisesRegex(ValueError, "duplicate index"):
            selected.observe({"type": "response.output_item.done", "output_index": 0, "item": original["output"][0]})
        for index in (True, -1, history.MAX_HISTORY_ITEMS):
            with self.subTest(index=index), self.assertRaises(ValueError):
                history.CompletedOutput().observe({"type": "response.output_item.done", "output_index": index})
        with self.assertRaisesRegex(ValueError, "cover terminal"):
            history.CompletedOutput().replay(response)
        response["output"][0]["id"] = "wrong_identity"
        with self.assertRaisesRegex(ValueError, "identity differs"):
            selected.replay(response)

    def test_streaming_item_completion_selection_is_explicit_and_does_not_log_payloads(self) -> None:
        source = completed(1)
        final = copy.deepcopy(source)
        final["output"][0]["encrypted_content"] = "terminal-only-opaque"
        events = [{"type": "response.output_item.done", "output_index": index, "item": item}
                  for index, item in enumerate(source["output"])]
        events.append({"type": "response.completed", "response": final})
        wire = "".join(f"data: {json.dumps(event)}\n\n" for event in events)
        transport = httpx.MockTransport(lambda request: httpx.Response(200, content=wire, request=request))
        for select in (False, True):
            output = io.StringIO()
            with self.subTest(select=select), httpx.Client(transport=transport) as client:
                with contextlib.redirect_stdout(output):
                    actual = recorder._send(client, {}, True, "http://fixture", display_payloads=False,
                                            replay_output_items=select)
                self.assertEqual(actual, source if select else final)
                self.assertNotIn("opaque", output.getvalue())

    def test_linear_and_branched_tool_history_preserves_exact_items(self) -> None:
        sent = []

        def send(_client, body, *_args, **kwargs):
            self.assertFalse(kwargs["display_payloads"])
            sent.append(copy.deepcopy(body))
            return completed(len(sent))

        with mock.patch.object(recorder, "_send", side_effect=send), mock.patch.object(
            recorder, "_prompt", side_effect=["start", "", "fork", "extra"]
        ):
            recorder.run_responses(
                object(), 3, "test", True, False, [(1, 3), (1, None)], "http://unused",
                manual_item_replay=True, tool_outputs={"lookup": "answer"},
                tool_choice_sequence=["required", "none", "auto", "none"],
            )

        self.assertEqual(len(sent), 4)
        self.assertTrue(all(not body["store"] and "previous_response_id" not in body for body in sent))
        self.assertEqual([body["tool_choice"] for body in sent], ["required", "none", "auto", "none"])
        prefix = sent[0]["input"] + completed(1)["output"]
        self.assertEqual(sent[1]["input"], prefix + [
            {"type": "function_call_output", "call_id": "call_1", "output": "answer"}
        ])
        for index in (2, 3):
            self.assertEqual(sent[index]["input"][:len(prefix)], prefix)
            self.assertEqual(sent[index]["input"][len(prefix)]["call_id"], "call_1")
            self.assertNotIn("rs_2", json.dumps(sent[index]["input"]))
            self.assertNotIn("rs_3", json.dumps(sent[index]["input"]))

    def test_checkpoints_are_immutable_and_validation_is_atomic(self) -> None:
        replay = history.ReplayHistory()
        first = replay.request_input(None, "start")
        response = completed(1)
        replay.record(1, first, response)
        first.clear()
        response["output"][0]["encrypted_content"] = "mutated"
        fork = replay.request_input(1, "fork")
        self.assertEqual(fork[1]["encrypted_content"], OPAQUE)
        fork.clear()
        self.assertEqual(len(replay.request_input(1, "again")), 4)
        for status in (None, "failed", "incomplete", "in_progress"):
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "explicitly completed"):
                replay.record(2, [], {**completed(2), "status": status})
        replay.record(2, [], completed(2))
        with self.assertRaisesRegex(ValueError, "duplicate"):
            replay.record(2, [], completed(2))
        with self.assertRaisesRegex(ValueError, "no completed checkpoint"):
            replay.request_input(3, "missing")

    def test_history_byte_item_checkpoint_and_turn_limits(self) -> None:
        for limit, value, action in (
            ("MAX_HISTORY_BYTES", 3, lambda replay: replay.request_input(None, "éé")),
            ("MAX_HISTORY_ITEMS", 1, lambda replay: replay.record(1, [], completed(1))),
            ("MAX_CHECKPOINT_BYTES", 3, lambda replay: replay.record(1, [], completed(1))),
            ("MAX_TURNS", 0, lambda replay: replay.record(1, [], completed(1))),
        ):
            with self.subTest(limit=limit), mock.patch.object(history, limit, value):
                with self.assertRaises(ValueError) as caught:
                    action(history.ReplayHistory())
                self.assertNotIn(OPAQUE, str(caught.exception))
        with self.assertRaisesRegex(ValueError, "item objects"):
            history.encode_history([None])

    def test_cli_accepts_openai_manual_replay_and_preserves_websocket_no_store(self) -> None:
        for transport in ("http", "websocket"):
            with self.subTest(transport=transport), tempfile.TemporaryDirectory() as directory:
                with (
                    mock.patch.object(recorder, "_start_proxy", return_value=object()),
                    mock.patch.object(recorder, "_stop_proxy"),
                    mock.patch.object(recorder, "run_responses") as run,
                ):
                    result = CliRunner().invoke(recorder.main, [
                        "--mode", "responses", "--turns", "3", "--no-store", "--manual-item-replay",
                        "--transport", transport, "--model", "gpt-5.4-2026-03-05",
                        "--branch-from", "1", "--branch-turn-number", "3",
                        "--output", str(Path(directory) / "capture.yaml"),
                    ], env={"OPENAI_API_KEY": "test-only-not-a-credential"})
                self.assertEqual(result.exit_code, 0, result.output)
                self.assertIs(run.call_args.args[4], False)
                self.assertTrue(run.call_args.kwargs["manual_item_replay"])

    def test_invalid_cli_combinations_fail_before_starting_transport(self) -> None:
        cases = [
            ["--turns", "1"],
            ["--turns", "65", "--no-store"],
            ["--turns", "1", "--no-store", "--mode", "conv"],
            ["--turns", "2", "--no-store", "--branch-from", "2", "--branch-turn-number", "2"],
            ["--turns", "2", "--no-store", "--branch-from", "3"],
        ]
        for args in cases:
            with self.subTest(args=args), mock.patch.object(recorder, "_start_proxy") as start:
                result = CliRunner().invoke(recorder.main, [
                    "--mode", "responses", "--manual-item-replay", "--output", "unused.yaml", *args
                ])
                self.assertNotEqual(result.exit_code, 0)
                start.assert_not_called()

    def test_http_manual_replay_does_not_print_payloads(self) -> None:
        for streaming in (False, True):
            response = completed(1)
            event = {"type": "response.completed", "response": response}
            wire = f"data: {json.dumps(event)}\n\ndata: [DONE]\n\n"
            transport = httpx.MockTransport(lambda request: httpx.Response(
                200, content=wire if streaming else json.dumps(response), request=request
            ))
            output = io.StringIO()
            with self.subTest(streaming=streaming), httpx.Client(transport=transport) as client:
                with contextlib.redirect_stdout(output):
                    actual = recorder._send(client, {}, streaming, "http://fixture", display_payloads=False)
                self.assertEqual(actual, response)
                self.assertNotIn("private-opaque-state", output.getvalue())

    def test_websocket_stateless_capture_retains_bytes_but_does_not_log_them(self) -> None:
        response = completed(1)
        event = json.dumps({"type": "response.completed", "response": response})
        with tempfile.TemporaryDirectory() as directory:
            ws = mock.MagicMock()
            ws.__enter__.return_value = ws
            ws.receive_text.return_value = event
            output = io.StringIO()
            with (
                mock.patch.object(recorder, "WebSocketClient", return_value=ws),
                mock.patch.object(recorder, "_append_turn") as append,
                contextlib.redirect_stdout(output),
            ):
                actual = recorder._send_websocket(
                    {"store": False, "stream": True}, "https://fixture", {},
                    Path(directory) / "capture.yaml", display_payloads=False,
                )
            body = json.loads(ws.send_text.call_args.args[0])
            self.assertIs(body["store"], False)
            self.assertNotIn("stream", body)
            self.assertEqual(actual, response)
            self.assertEqual(append.call_args.args[1]["response"]["websocket"], [event])
            self.assertNotIn("private-opaque-state", output.getvalue())

    def test_websocket_incomplete_stops_capture_and_oversized_frame_fails_before_read(self) -> None:
        event = json.dumps({"type": "response.incomplete", "response": {"status": "incomplete"}})
        ws = mock.MagicMock()
        ws.__enter__.return_value = ws
        ws.receive_text.side_effect = [event, AssertionError("must stop at terminal")]
        with (
            mock.patch.object(recorder, "WebSocketClient", return_value=ws),
            mock.patch.object(recorder, "_append_turn"),
        ):
            result = recorder._send_websocket({}, "https://fixture", {}, Path("unused.yaml"), False)
        self.assertEqual(result, {"status": "incomplete"})
        client = recorder.WebSocketClient("ws://fixture", {})
        with mock.patch.object(client, "_read_exact", side_effect=[
            b"\x81\x7f", struct.pack("!Q", recorder.MAX_WEBSOCKET_MESSAGE_BYTES + 1)
        ]) as read:
            with self.assertRaisesRegex(ValueError, "byte limit"):
                client.receive_text()
            self.assertEqual(read.call_count, 2)


if __name__ == "__main__":
    unittest.main()
