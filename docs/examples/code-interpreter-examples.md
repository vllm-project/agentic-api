# Code Interpreter Examples

Use the built-in Code Interpreter to perform calculations, data analysis, and more.

## Prerequisite

Ensure your client request includes the tool definition and `include` parameter:

```python
# Common setup for all examples
client = OpenAI(...)
kwargs = {
    "model": "meta-llama/Llama-3.2-3B-Instruct",
    "tools": [{"type": "code_interpreter"}],
    "include": ["code_interpreter_call.outputs"]
}
```

### Output semantics (what you’ll see in `code_interpreter_call.outputs`)

When `include=["code_interpreter_call.outputs"]` is present, the gateway populates `outputs` with up to two log entries:

1. Aggregated stdout/stderr (e.g. everything from `print(...)`).
1. The final expression display value (if the last statement is a bare expression).

If your code ends with `print(...)` and no final expression, you’ll typically only see the first logs entry.

## 1. Mathematical Calculation

The model can solve math problems by writing Python code, which avoids the common arithmetic errors LLMs make.

**Prompt:** "Calculate the factorial of 50."

**Model Execution:**

```python
import math
print(math.factorial(50))
```

**Output:**

```text
30414093201713378043612608166064768844377641568960512000000000000
```

## 2. Data Analysis with Pandas

The environment includes `pandas` and `numpy`.

**Prompt:** "Create a DataFrame with 5 random numbers and calculate their mean."

**Model Execution:**

```python
import pandas as pd
import numpy as np

df = pd.DataFrame({'values': np.random.rand(5)})
print(f"Mean: {df['values'].mean()}")
print(df)
```

## 3. String Processing

Use Python's powerful string manipulation capabilities.

**Prompt:** "Reverse the string 'Hello World' and count the vowels."

**Model Execution:**

```python
s = "Hello World"
reversed_s = s[::-1]
vowel_count = sum(1 for char in s.lower() if char in 'aeiou')
print(f"Reversed: {reversed_s}")
print(f"Vowels: {vowel_count}")
```

## 4. HTTP Requests

The sandbox allows HTTP requests (via a proxy) using `httpx`.

**Prompt:** "Fetch the current time from an API."

**Model Execution:**

```python
import httpx
r = httpx.get("https://worldtimeapi.org/api/ip")
data = r.json()
print(f"Current time: {data['datetime']}")
```

(Note: Network access depends on your deployment configuration.)
