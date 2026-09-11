"""Strict Files/Vector Stores/Responses contracts against the actual local binary.

Run: uv run --no-project --python 3.13 --with openai==3.13.0 python
     scripts/tests/file-search-sdk-test.py /path/to/agentic-server
All provider and SDK traffic uses owned loopback fixtures and sanitized environments.
"""

import hashlib
import signal
import sys
import tempfile
import time
import unittest

from sdk_file_search_fixtures import Gateway, ResponsesModel, RetrievalModels

import httpx2 as httpx
from openai import BadRequestError, OpenAI


class FileSearchSDKContract(unittest.TestCase):
    client: OpenAI
    models: RetrievalModels

    def setUp(self):
        # Per-request timeouts cannot bound a stream that keeps sending partial
        # bytes. Bound the entire test too, including SDK decoding and polling.
        def expired(_signal, _frame):
            raise TimeoutError("SDK contract test exceeded its 90-second deadline")

        previous = signal.signal(signal.SIGALRM, expired)
        self.addCleanup(signal.signal, signal.SIGALRM, previous)
        self.addCleanup(signal.setitimer, signal.ITIMER_REAL, 0)
        signal.setitimer(signal.ITIMER_REAL, 90)
        self.files = []
        self.stores = []

    def tearDown(self):
        self.models.unblock()
        for store in self.stores:
            self.client.vector_stores.delete(store)
        for file in self.files:
            self.client.files.delete(file)

    def upload(
        self,
        name="policy.txt",
        text=b"The lunar return policy permits thirty days.",
        **kwargs,
    ):
        obj = self.client.files.create(
            file=(name, text), purpose=kwargs.pop("purpose", "assistants"), **kwargs
        )
        self.files.append(obj.id)
        return obj

    def store(self, **kwargs):
        obj = self.client.vector_stores.create(**kwargs)
        self.stores.append(obj.id)
        return obj

    def wait_for_ingestion(self, initial, retrieve, total_seconds=10):
        deadline = time.monotonic() + total_seconds
        current = initial
        while current.status == "in_progress":
            remaining = deadline - time.monotonic()
            self.assertGreater(
                remaining,
                0,
                f"Ingestion {current.id} did not finish within {total_seconds} seconds",
            )
            time.sleep(min(0.1, remaining))
            current = retrieve(min(remaining, 3))
        return current

    def test_files_purpose_expiration_and_download(self):
        payload = b"\x00\x01binary\xff"
        file = self.upload(
            "data.bin",
            payload,
            purpose="user_data",
            expires_after={"anchor": "created_at", "seconds": 3600},
        )
        self.assertEqual(file.expires_at, file.created_at + 3600)
        self.assertEqual(self.client.files.content(file.id).read(), payload)
        page = self.client.files.list(purpose="user_data", limit=10000)
        self.assertIn(file.id, [item.id for item in page.data])
        self.assertTrue(all(item.purpose == "user_data" for item in page.data))

    def test_file_upload_above_old_limit_and_streamed_download(self):
        size = 21 * 1024 * 1024
        with tempfile.TemporaryFile() as source:
            source.seek(size - 1)
            source.write(b"z")
            source.seek(0)
            expected = hashlib.file_digest(source, "sha256").hexdigest()
            source.seek(0)
            file = self.client.files.create(
                file=("large.bin", source), purpose="user_data"
            )
            self.files.append(file.id)
        self.assertEqual(file.bytes, size)
        received = 0
        digest = hashlib.sha256()
        with self.client.files.with_streaming_response.content(file.id) as response:
            for chunk in response.iter_bytes(chunk_size=65536):
                received += len(chunk)
                digest.update(chunk)
        self.assertEqual(received, size)
        self.assertEqual(digest.hexdigest(), expected)

    def test_file_delete_response_schema(self):
        file = self.upload()
        self.files.remove(file.id)
        deleted = self.client.files.delete(file.id)
        self.assertEqual(deleted.object, "file")
        self.assertTrue(deleted.deleted)

    def test_store_nullable_updates_and_expiry(self):
        store = self.store(
            name="before",
            metadata=None,
            expires_after={"anchor": "last_active_at", "days": 1},
        )
        self.assertEqual(store.expires_at, store.last_active_at + 86400)
        changed = self.client.vector_stores.update(
            store.id, name="after", metadata={"department": "support"}
        )
        self.assertEqual(changed.name, "after")
        self.assertEqual(changed.metadata, {"department": "support"})
        self.assertIsNotNone(changed.expires_after)
        cleared = self.client.vector_stores.update(
            store.id, expires_after=None, metadata=None
        )
        self.assertIsNone(cleared.expires_after)
        self.assertEqual(cleared.name, "after")
        self.assertFalse(cleared.metadata)

    def test_file_attributes_and_parsed_content(self):
        file = self.upload()
        store = self.store()
        attachment = self.client.vector_stores.files.create(
            vector_store_id=store.id, file_id=file.id, attributes=None
        )
        self.wait_for_ingestion(
            attachment,
            lambda timeout: self.client.vector_stores.files.retrieve(
                file.id, vector_store_id=store.id, timeout=timeout
            ),
        )
        changed = self.client.vector_stores.files.update(
            file.id, vector_store_id=store.id, attributes={"department": "support"}
        )
        self.assertEqual(changed.attributes, {"department": "support"})
        content = self.client.vector_stores.files.content(
            file.id, vector_store_id=store.id
        )
        self.assertIn(
            "lunar return policy", " ".join(item.text or "" for item in content.data)
        )
        hit = self.client.vector_stores.search(
            store.id,
            query="lunar",
            filters={"type": "eq", "key": "department", "value": "support"},
        )
        self.assertEqual([item.file_id for item in hit.data], [file.id])
        self.client.vector_stores.files.update(
            file.id, vector_store_id=store.id, attributes=None
        )
        miss = self.client.vector_stores.search(
            store.id,
            query="lunar",
            filters={"type": "eq", "key": "department", "value": "support"},
        )
        self.assertEqual(miss.data, [])

    def test_metadata_limits_count_unicode_characters(self):
        key = "界" * 64
        value = "文" * 512
        store = self.store(metadata={key: value})
        self.assertEqual(
            self.client.vector_stores.retrieve(store.id).metadata, {key: value}
        )
        changed = self.client.vector_stores.update(store.id, metadata={key: "章" * 512})
        self.assertEqual(changed.metadata, {key: "章" * 512})
        with self.assertRaises(BadRequestError):
            self.store(metadata={key + "界": value})
        with self.assertRaises(BadRequestError):
            self.client.vector_stores.update(store.id, metadata={key: value + "文"})
        self.assertEqual(
            self.client.vector_stores.retrieve(store.id).metadata, changed.metadata
        )

    def test_attribute_and_filter_limits_count_unicode_characters(self):
        key = "界" * 64
        value = "文" * 512
        file = self.upload()
        store = self.store()
        attachment = self.client.vector_stores.files.create(
            vector_store_id=store.id, file_id=file.id, attributes={key: value}
        )
        done = self.wait_for_ingestion(
            attachment,
            lambda timeout: self.client.vector_stores.files.retrieve(
                file.id, vector_store_id=store.id, timeout=timeout
            ),
        )
        self.assertEqual(done.attributes, {key: value})
        hits = self.client.vector_stores.search(
            store.id, query="lunar", filters={"type": "eq", "key": key, "value": value}
        )
        self.assertEqual([item.file_id for item in hits.data], [file.id])
        self.assertEqual(hits.data[0].attributes, {key: value})
        with self.assertRaises(BadRequestError):
            self.client.vector_stores.files.update(
                file.id, vector_store_id=store.id, attributes={key + "界": value}
            )
        with self.assertRaises(BadRequestError):
            self.client.vector_stores.files.update(
                file.id, vector_store_id=store.id, attributes={key: value + "文"}
            )
        self.assertEqual(
            self.client.vector_stores.files.retrieve(
                file.id, vector_store_id=store.id
            ).attributes,
            {key: value},
        )

    def test_search_named_ranker_and_result_schema(self):
        file = self.upload()
        store = self.store(file_ids=[file.id])
        hits = self.client.vector_stores.search(
            store.id,
            query=["lunar"],
            ranking_options={"ranker": "default-2024-11-15"},
            max_num_results=1,
        )
        self.assertEqual([hit.file_id for hit in hits.data], [file.id])
        self.assertTrue(0 <= hits.data[0].score <= 1)
        self.assertEqual(hits.model_dump()["search_query"], ["lunar"])

    def test_batch_per_file_options_and_list_filter(self):
        first = self.upload("first.txt", b"First lunar document.")
        second = self.upload("second.txt", b"Second orbital document.")
        store = self.store()
        batch = self.client.vector_stores.file_batches.create(
            store.id,
            files=[
                {"file_id": first.id, "attributes": {"kind": "lunar"}},
                {
                    "file_id": second.id,
                    "chunking_strategy": {
                        "type": "static",
                        "static": {
                            "max_chunk_size_tokens": 100,
                            "chunk_overlap_tokens": 0,
                        },
                    },
                },
            ],
        )
        batch = self.wait_for_ingestion(
            batch,
            lambda timeout: self.client.vector_stores.file_batches.retrieve(
                batch.id, vector_store_id=store.id, timeout=timeout
            ),
        )
        self.assertEqual(batch.object, "vector_store.files_batch")
        self.assertEqual(batch.status, "completed")
        self.assertEqual(batch.file_counts.completed, 2)
        self.assertEqual(batch.file_counts.total, 2)
        page = self.client.vector_stores.file_batches.list_files(
            batch.id, vector_store_id=store.id, filter="completed", limit=1, order="asc"
        )
        self.assertEqual(len(page.data), 1)
        self.assertTrue(page.has_more)
        following = self.client.vector_stores.file_batches.list_files(
            batch.id,
            vector_store_id=store.id,
            filter="completed",
            limit=1,
            order="asc",
            after=page.data[-1].id,
        )
        self.assertEqual(len(following.data), 1)
        self.assertNotEqual(following.data[0].id, page.data[0].id)
        filtered = self.client.vector_stores.files.list(store.id, filter="failed")
        self.assertEqual(filtered.data, [])

    def test_evals_raw_response_despite_sdk_enum_omission(self):
        # SDK3.13.0 accepts evals input but omits it from FileObject's purpose enum.
        raw = self.client.files.with_raw_response.create(
            file=("evals.bin", b"evaluation payload"), purpose="evals"
        ).http_response.json()
        self.files.append(raw["id"])
        self.assertEqual(raw["object"], "file")
        self.assertEqual(raw["purpose"], "evals")
        got = self.client.files.with_raw_response.retrieve(
            raw["id"]
        ).http_response.json()
        self.assertEqual(got["purpose"], "evals")
        page = self.client.files.with_raw_response.list(
            purpose="evals", limit=10000
        ).http_response.json()
        self.assertIn(raw["id"], [item["id"] for item in page["data"]])
        self.assertTrue(all(item["purpose"] == "evals" for item in page["data"]))
        self.assertEqual(
            self.client.files.content(raw["id"]).read(), b"evaluation payload"
        )

    def test_search_rewrite_reaches_embedding_and_reports_actual_query(self):
        file = self.upload()
        store = self.store(file_ids=[file.id])
        offset = len(self.models.snapshot())
        hits = self.client.vector_stores.search(
            store.id,
            query="what is the return window",
            rewrite_query=True,
            ranking_options={"ranker": "default-2024-11-15"},
        )
        self.assertEqual(hits.search_query, ["lunar return policy"])
        self.assertEqual([hit.file_id for hit in hits.data], [file.id])
        requests = self.models.snapshot()[offset:]
        self.assertTrue(any(path == "/v1/chat/completions" for path, _ in requests))
        self.assertTrue(
            any(
                path == "/v1/embeddings" and "lunar return policy" in body["input"]
                for path, body in requests
            )
        )
        self.assertTrue(any(path == "/rerank" for path, _ in requests))

    def test_batch_cancel_while_embedding_is_blocked(self):
        file = self.upload()
        store = self.store()
        self.models.block_embeddings()
        batch = self.client.vector_stores.file_batches.create(
            store.id, file_ids=[file.id]
        )
        self.assertEqual(batch.status, "in_progress")
        self.assertTrue(
            self.models.entered.wait(timeout=5), "Batch worker did not call embeddings"
        )
        cancelled = self.client.vector_stores.file_batches.cancel(
            batch.id, vector_store_id=store.id
        )
        self.assertEqual(cancelled.status, "cancelled")
        self.models.unblock()
        # Verify durable cancellation through the API; runtime fencing has separate Rust tests.
        final = self.client.vector_stores.file_batches.retrieve(
            batch.id, vector_store_id=store.id
        )
        self.assertEqual(final.status, "cancelled")
        self.assertEqual(final.file_counts.cancelled, 1)
        self.assertEqual(final.file_counts.completed, 0)
        members = self.client.vector_stores.file_batches.list_files(
            batch.id, vector_store_id=store.id, filter="cancelled"
        )
        self.assertEqual([item.id for item in members.data], [file.id])
        self.assertEqual(
            self.client.vector_stores.search(store.id, query="lunar").data, []
        )
        self.assertEqual(
            self.client.files.content(file.id).read(),
            b"The lunar return policy permits thirty days.",
        )

    def test_streamed_citation_events_match_strict_completed_response(self):
        request_offset = len(self.upstream.requests)
        file = self.upload()
        self.upstream.file_id = file.id
        store = self.store(file_ids=[file.id])
        with self.client.responses.create(
            model="test-model",
            input="What is the lunar return policy?",
            tools=[{"type": "file_search", "vector_store_ids": [store.id]}],
            include=["file_search_call.results"],
            stream=True,
        ) as stream:
            events = []
            stream_deadline = time.monotonic() + 30
            for event in stream:
                assert len(events) < 256 and time.monotonic() < stream_deadline, (
                    "Stream contract exceeded its bounds"
                )
                events.append(event.model_dump(mode="json"))
        assert [event["sequence_number"] for event in events] == list(
            range(len(events))
        )
        completed = [event for event in events if event["type"] == "response.completed"]
        assert len(completed) == 1, events
        output = completed[0]["response"]["output"]
        message_index = next(
            i for i, item in enumerate(output) if item["type"] == "message"
        )
        message = output[message_index]
        annotations = message["content"][0]["annotations"]
        assert len(annotations) == 1 and annotations[0]["file_id"] == file.id, (
            annotations
        )
        added = [
            event
            for event in events
            if event["type"] == "response.output_text.annotation.added"
        ]
        assert len(added) == 1, (
            f"Expected one annotation event matching final citations; received {len(added)}"
        )
        event = added[0]
        assert event["annotation"] == annotations[0]
        assert (
            event["item_id"],
            event["output_index"],
            event["content_index"],
            event["annotation_index"],
        ) == (message["id"], message_index, 0, 0)
        content_done_index = next(
            i for i, e in enumerate(events) if e["type"] == "response.content_part.done"
        )
        assert events.index(event) < content_done_index
        assert len(self.upstream.requests) - request_offset == 2
        response = completed[0]["response"]
        self.assertFalse(response["parallel_tool_calls"])
        self.assertEqual(response["tool_choice"], "auto")
        self.assertEqual(response["tools"][0]["type"], "file_search")
        self.assertEqual(response["tools"][0]["vector_store_ids"], [store.id])
        # Native response retrieval is not a route in this server. Persistence is
        # exercised through continuation; Rust tests inspect stored metadata.
        continued = self.client.responses.create(
            model="test-model",
            input="Explain the policy",
            previous_response_id=response["id"],
            parallel_tool_calls=True,
            tool_choice="none",
        )
        self.assertTrue(continued.parallel_tool_calls)
        self.assertEqual(continued.tool_choice, "none")
        self.assertEqual(continued.tools[0].type, "file_search")
        self.assertEqual(continued.output[0].content[0].annotations[0].file_id, file.id)

    def test_restart_resumes_ingestion_after_orderly_shutdown(self):
        file = self.upload()
        store = self.store()
        self.models.block_embeddings()
        batch = self.client.vector_stores.file_batches.create(
            store.id, file_ids=[file.id]
        )
        self.assertTrue(
            self.models.entered.wait(timeout=5),
            "worker must own a blocked model request",
        )
        # stop() asserts zero exit and fails if emergency kill was necessary.
        self.gateway.stop()
        self.models.unblock()
        self.gateway.start()
        batch = self.wait_for_ingestion(
            batch,
            lambda timeout: self.client.vector_stores.file_batches.retrieve(
                batch.id, vector_store_id=store.id, timeout=timeout
            ),
        )
        self.assertEqual(batch.status, "completed")
        self.assertEqual(batch.file_counts.completed, 1)
        self.assertEqual(batch.file_counts.total, 1)
        members = self.client.vector_stores.file_batches.list_files(
            batch.id, vector_store_id=store.id
        )
        self.assertEqual([item.id for item in members.data], [file.id])
        self.assertEqual(
            self.client.files.content(file.id).read(),
            b"The lunar return policy permits thirty days.",
        )

    def test_crash_restart_recovers_expired_worker_claim(self):
        file = self.upload()
        store = self.store()
        self.models.block_embeddings()
        batch = self.client.vector_stores.file_batches.create(
            store.id, file_ids=[file.id]
        )
        self.assertTrue(self.models.entered.wait(timeout=5))
        self.gateway.crash()
        self.models.unblock()
        self.gateway.start()
        # A hard crash cannot release its claim; wait for the real 30-second lease.
        batch = self.wait_for_ingestion(
            batch,
            lambda timeout: self.client.vector_stores.file_batches.retrieve(
                batch.id, vector_store_id=store.id, timeout=timeout
            ),
            total_seconds=45,
        )
        self.assertEqual(batch.status, "completed")
        self.assertEqual(batch.file_counts.completed, 1)
        self.assertEqual(batch.file_counts.total, 1)
        found = self.client.vector_stores.search(store.id, query="lunar")
        self.assertEqual([item.file_id for item in found.data], [file.id])

    def test_partial_batch_failure_keeps_valid_member(self):
        valid = self.upload()
        invalid = self.upload("unsupported.bin", b"unsupported")
        store = self.store()
        initial = self.client.vector_stores.file_batches.create(
            store.id, file_ids=[valid.id, invalid.id]
        )
        batch = self.wait_for_ingestion(
            initial,
            lambda timeout: self.client.vector_stores.file_batches.retrieve(
                initial.id, vector_store_id=store.id, timeout=timeout
            ),
        )
        self.assertEqual(batch.status, "completed")
        self.assertEqual(batch.file_counts.completed, 1)
        self.assertEqual(batch.file_counts.failed, 1)
        failed = self.client.vector_stores.file_batches.list_files(
            batch.id, vector_store_id=store.id, filter="failed"
        )
        self.assertEqual([item.id for item in failed.data], [invalid.id])
        self.assertEqual(failed.data[0].last_error.code, "unsupported_file")

    def test_files_purposes_and_pagination(self):
        for purpose in ["assistants", "batch", "fine-tune", "vision", "user_data"]:
            with self.subTest(purpose=purpose):
                first = self.upload(purpose=purpose)
                second = self.upload(purpose=purpose)
                self.assertEqual(self.client.files.retrieve(first.id).purpose, purpose)
                if purpose == "batch":
                    self.assertEqual(first.expires_at, first.created_at + 30 * 86400)
                page = self.client.files.list(purpose=purpose, limit=1, order="asc")
                self.assertTrue(page.has_more)
                self.assertEqual([item.id for item in page.data], [first.id])
                following = self.client.files.list(
                    purpose=purpose, limit=1, order="asc", after=first.id
                )
                self.assertEqual([item.id for item in following.data], [second.id])


def main():
    binary = sys.argv.pop(1)
    with (
        ResponsesModel() as upstream,
        RetrievalModels() as models,
        tempfile.TemporaryDirectory(prefix="agentic-sdk-contract-") as directory,
    ):
        models.configure(directory)
        with Gateway(binary, directory, upstream) as gateway:
            with OpenAI(
                base_url=f"{gateway.base}/v1",
                api_key="sdk-contract-test",
                max_retries=0,
                http_client=httpx.Client(trust_env=False),
                timeout=10,
                _strict_response_validation=True,
            ) as client:
                FileSearchSDKContract.client = client
                FileSearchSDKContract.models = models
                FileSearchSDKContract.upstream = upstream
                FileSearchSDKContract.gateway = gateway
                outcome = unittest.main(exit=False)
                return 0 if outcome.result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
