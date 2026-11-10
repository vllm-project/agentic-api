# File-search citation fixtures

`citations.json` contains synthetic regression inputs and exact expected annotations.
It is not a recorded OpenAI/OGX response and does not establish live provider parity.

Cases are adapted from OGX at commit
[`e861635d87a8bebb8f80e60edd560dde923f2c43`](https://github.com/ogx-ai/ogx/tree/e861635d87a8bebb8f80e60edd560dde923f2c43):

- [`test_response_conversion_utils.py`](https://github.com/ogx-ai/ogx/blob/e861635d87a8bebb8f80e60edd560dde923f2c43/tests/unit/providers/responses/builtin/test_response_conversion_utils.py):
  explicit, repeated, multiple, unknown, and mixed known/unknown citations.
- [`test_openai_responses_tools.py`](https://github.com/ogx-ai/ogx/blob/e861635d87a8bebb8f80e60edd560dde923f2c43/tests/unit/providers/responses/builtin/test_openai_responses_tools.py):
  citations in completed content/output items and markers split across streamed deltas.
- [`file_search_final_answer_completion.yaml`](https://github.com/ogx-ai/ogx/blob/e861635d87a8bebb8f80e60edd560dde923f2c43/tests/unit/providers/responses/builtin/fixtures/file_search_final_answer_completion.yaml):
  the climate-answer sentence and fixture file IDs.

The applicable OGX license is retained in [LICENSE.OGX](LICENSE.OGX).

The fixtures use Agentic API's documented `【file-id】` markers. Text remains unchanged
through streaming and finalization. Retrieved files without an explicit citation receive
no annotation. Additional cases cover Unicode character offsets, malformed markers,
empty retrieval, existing citations, duplicate removal, canonical filenames, and
preservation of other annotation kinds.

`native_annotations.json` is a synthetic multipart Responses payload and SSE event
sequence using the shapes in the official
[annotation-added event reference](https://developers.openai.com/api/reference/resources/responses/streaming-events#response.output_text.annotation.added).
It covers file, URL, container-file, file-path, and future annotation kinds. These
annotations must survive execution when the gateway does not own `file_search`,
including a client function with that name.

The Rust `file_search_tool_test` runs the same cases with and without result inclusion,
over JSON and SSE, through real local ingestion/retrieval, response persistence, and
`previous_response_id` continuation. Runtime-generated file IDs replace the fixed fixture
IDs; annotation offsets are adjusted by expanding the corresponding fixture text prefixes.
Each generated answer is streamed one Unicode scalar at a time, splitting citation
delimiters and file IDs across chunks.

Run:

```bash
cargo test -p agentic-server-core --test file_search_tool_test

cargo build -p agentic-server --bin agentic-server
uv run --no-project --with openai==3.8.0 python scripts/tests/openai-file-search-client-test.py
```

The existing OpenAI client CI check runs these same fixtures through the gateway's
HTTP routes and the SDK's streaming accumulator, including retrieval of stored
responses and continuation. The inference server is the existing local replay
server with synthetic turns; no provider credential is required.

Live reference captures must use the recorder workflow in
[`../../cassettes/README.md`](../../cassettes/README.md); do not turn these synthetic
cases into hand-authored capture YAML.
