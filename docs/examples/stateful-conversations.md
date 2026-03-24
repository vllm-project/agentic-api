# Stateful Conversation Examples

Use `previous_response_id` to maintain conversation history efficiently.

## Basic Chat

This example simulates a conversation between a user and an assistant.

### Turn 1: User Greeting

```python
# Initial request
response_1 = client.responses.create(
    model="meta-llama/Llama-3.2-3B-Instruct",
    input=[
        {"role": "system", "content": "You are a helpful pirate."},
        {"role": "user", "content": "Hello!"}
    ]
)

print(response_1.output[0].content)
# "Ahoy there, matey!"
```

### Turn 2: User Follow-up

For the next turn, we **do not** send the previous messages. We only send the new input and the ID of the last response.

```python
response_2 = client.responses.create(
    model="meta-llama/Llama-3.2-3B-Instruct",
    previous_response_id=response_1.id,
    input=[{"role": "user", "content": "Where is the treasure?"}]
)

print(response_2.output[0].content)
# "X marks the spot on the map!"
```

### Turn 3: Assistant Continues

We continue the chain using `response_2.id`.

```python
response_3 = client.responses.create(
    model="meta-llama/Llama-3.2-3B-Instruct",
    previous_response_id=response_2.id,
    input=[{"role": "user", "content": "Can you show me the map?"}]
)

print(response_3.output[0].content)
# "Aye, here it be..."
```

## Changing Instructions

You can change the system prompt mid-conversation. The `instructions` parameter (system prompt) is **not** carried over by `previous_response_id`. You must provide it if you want it to persist or change.

```python
# Change persona to a ninja
response_4 = client.responses.create(
    model="meta-llama/Llama-3.2-3B-Instruct",
    previous_response_id=response_3.id,
    input=[{"role": "user", "content": "Wait, be quiet now."}],
    instructions="You are a stealthy ninja."
)

print(response_4.output[0].content)
# "(Whispers) Understood. I vanish into the shadows."
```
