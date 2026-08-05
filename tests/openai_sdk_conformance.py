"""Conformance checks using the public OpenAI Python Conversations and Responses clients.

Run this against a running gateway configured with a model that can answer a short
Responses request.  The script intentionally uses only public SDK resources.
"""

from __future__ import annotations

import os

from openai import BadRequestError, NotFoundError, OpenAI


BASE_URL = os.environ.get("AGENTIC_API_BASE_URL", "http://127.0.0.1:8000/v1")
API_KEY = os.environ.get("OPENAI_API_KEY", "test-key")
MODEL = os.environ.get("OPENAI_TEST_MODEL", "test-model")


def assert_not_found(operation) -> None:
    try:
        operation()
    except NotFoundError as error:
        assert error.status_code == 404
    else:
        raise AssertionError("operation unexpectedly succeeded")


def main() -> None:
    client = OpenAI(base_url=BASE_URL, api_key=API_KEY, max_retries=0)

    conversation = client.conversations.create(
        metadata={"suite": "openai-sdk"},
        items=[
            {"type": "message", "role": "user", "content": "hello"},
            {
                "type": "function_call",
                "id": "fc_sdk",
                "call_id": "call_sdk",
                "name": "lookup",
                "arguments": "{}",
                "status": "completed",
            },
            {"type": "function_call_output", "call_id": "call_sdk", "output": "done"},
            {
                "type": "custom_tool_call",
                "id": "ctc_sdk",
                "call_id": "custom_sdk",
                "name": "echo",
                "input": "hello",
                "status": "completed",
            },
            {"type": "custom_tool_call_output", "call_id": "custom_sdk", "output": "done"},
        ],
    )
    assert conversation.object == "conversation"
    assert conversation.metadata == {"suite": "openai-sdk"}

    retrieved = client.conversations.retrieve(conversation.id)
    assert retrieved.id == conversation.id
    updated = client.conversations.update(conversation.id, metadata={"suite": "openai-sdk-updated"})
    assert updated.metadata == {"suite": "openai-sdk-updated"}

    first_page = client.conversations.items.list(conversation.id, order="asc", limit=1)
    assert len(first_page.data) == 1
    assert first_page.has_more
    first_item_id = first_page.data[0].id
    second_page = client.conversations.items.list(
        conversation.id,
        order="asc",
        after=first_item_id,
        limit=1,
    )
    assert len(second_page.data) == 1
    assert second_page.data[0].id != first_item_id
    descending_page = client.conversations.items.list(conversation.id, limit=1)
    assert len(descending_page.data) == 1

    appended = client.conversations.items.create(
        conversation.id,
        items=[{"type": "message", "role": "user", "content": "follow up"}],
    )
    appended_item_id = appended.data[0].id
    assert client.conversations.items.retrieve(appended_item_id, conversation_id=conversation.id).id == appended_item_id
    assert client.conversations.items.delete(appended_item_id, conversation_id=conversation.id).id == conversation.id
    assert_not_found(lambda: client.conversations.items.retrieve(appended_item_id, conversation_id=conversation.id))

    response_conversation = client.conversations.create(metadata={"suite": "openai-sdk-responses"})
    first_response = client.responses.create(
        model=MODEL,
        input="first turn",
        conversation=response_conversation.id,
        store=True,
    )
    assert first_response.conversation is not None
    assert first_response.conversation.id == response_conversation.id

    second_response = client.responses.create(
        model=MODEL,
        input="second turn",
        conversation=response_conversation.id,
        store=False,
    )
    assert second_response.conversation is not None
    assert second_response.conversation.id == response_conversation.id

    try:
        client.responses.create(
            model=MODEL,
            input="ambiguous state",
            conversation=response_conversation.id,
            previous_response_id=first_response.id,
            store=True,
        )
    except BadRequestError as error:
        assert error.status_code == 400
    else:
        raise AssertionError("conversation and previous_response_id should be rejected together")

    deleted = client.conversations.delete(conversation.id)
    assert deleted.deleted
    assert_not_found(lambda: client.conversations.retrieve(conversation.id))


if __name__ == "__main__":
    main()
