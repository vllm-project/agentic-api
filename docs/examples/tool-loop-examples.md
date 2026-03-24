# Custom Tool Loop Examples

How to implement the client-side tool loop for custom functions.

## Weather Example

This example demonstrates a complete cycle: Model requests tool -> Client executes -> Model uses result.

### 1. Define the Tool

First, tell the model about the tool.

```python
tools = [
    {
        "type": "function",
        "function": {
            "name": "get_current_weather",
            "description": "Get the current weather in a given location",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city and state, e.g. San Francisco, CA",
                    },
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]},
                },
                "required": ["location"],
            },
        },
    }
]
```

### 2. Initial Request

```python
response = client.responses.create(
    model="meta-llama/Llama-3.2-3B-Instruct",
    input=[{"role": "user", "content": "What's the weather in Boston?"}],
    tools=tools,
    tool_choice="auto",
)

# Store the response ID for the next step
response_id = response.id
```

### 3. Handle Tool Call

Check if the model wants to call a function.

```python
output_item = response.output[0]

if output_item.type == "function_call":
    tool_name = output_item.name
    tool_args = output_item.arguments
    call_id = output_item.call_id

    print(f"Model called {tool_name} with {tool_args}")

    # In a real app, you would parse args and call your API
    # function_response = my_weather_api(location="Boston")
    function_response = '{"temperature": "22", "unit": "celsius", "description": "Sunny"}'
```

### 4. Submit Result

Send the tool output back to the model using `previous_response_id`.

```python
final_response = client.responses.create(
    model="meta-llama/Llama-3.2-3B-Instruct",
    previous_response_id=response_id,
    input=[
        {
            "type": "function_call_output",
            "call_id": call_id,
            "output": function_response
        }
    ]
)

print(final_response.output[0].content)
# "The weather in Boston is 22°C and sunny."
```

For MCP examples, see [MCP Examples](hosted-mcp-examples.md).
