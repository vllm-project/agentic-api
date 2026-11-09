# Provider parity and recorded evidence

## Read primary documentation before deciding the contract

Use official documentation for the affected operation, not an SDK type alone, an issue description, a blog, or model
memory. Open the actual page and record the relevant link in the review/design notes. Check defaults, absent versus
null fields, item unions, tool execution ownership, continuation, streaming lifecycle, errors and limits as applicable.

| Source | Starting point |
| --- | --- |
| OpenAI Responses | [Responses reference](https://developers.openai.com/api/reference/responses/overview) |
| OpenAI tools | [Tools guide](https://developers.openai.com/api/docs/guides/tools) |
| OpenAI Conversations/continuation | [Conversation state guide](https://developers.openai.com/api/docs/guides/conversation-state); follow its current Conversations reference links |
| Anthropic Messages | [Messages guide](https://platform.claude.com/docs/en/build-with-claude/working-with-messages); follow its API reference |
| Anthropic tools | [Tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview) |
| Anthropic streaming | [Streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming) |
| Rust design | [Rust Cookbook](https://rust-lang-nursery.github.io/rust-cookbook/), [API Guidelines](https://rust-lang.github.io/api-guidelines/) |
| Rust serialization | [Serde derive](https://serde.rs/derive.html), then enum representation/field attributes or custom deserialization when required |

Use available official-documentation tools, or official-site browsing. For an OpenAI-facing change, read OpenAI's
contract; for Messages, read Anthropic's; for a translation between them, read both. Distinguish that public contract
from the actual vLLM/other backend version and gateway policy. Confirm routing and normalization in code before
attributing a provider error to the gateway. Internal Rust-only changes need the design sources, not unrelated API
research. If documentation is unavailable or ambiguous, state what remains unverified.

For behavior not established by documentation or existing recordings, reproduce against the relevant provider and
capture it through the recorder. Provider model/version, API headers, transport, request and response shape matter.
Do not invent a parity rule to fit the gateway's current implementation. Document intentional gateway extensions or
unsupported features separately; do not claim parity for capabilities the gateway does not implement.

## Decide whether a new recording is needed

- **New/ambiguous external behavior:** record the provider reference and corresponding gateway behavior; add replay
  assertions for the contract being implemented. Existing upstream/vLLM traffic alone is not OpenAI/Anthropic parity.
- **Already established behavior:** identify and reuse the exact existing cassette and assertions; extend the replay
  test if the current gateway path is not exercised. Do not record duplicates merely because a file changed.
- **Pure typing/ownership/refactor:** existing cassette replay plus targeted regression tests can be sufficient when
  externally visible behavior is unchanged. State that decision.
- **Missing credentials/provider/gateway:** prepare the recorder invocation and test case, report the missing evidence,
  and do independent work. Do not fabricate captures, silently skip required replay assertions, or declare parity.

## Existing Python recorder and shell workflows

All commands below run from the repository root. First read
`crates/agentic-server-core/tests/cassettes/README.md` and the relevant script/header; their current options and scenario
definitions are authoritative. Reuse/extend those workflows instead of creating another recorder or proxy.

| Workflow | Script and fixtures |
| --- | --- |
| Shared HTTP capture | `crates/agentic-server-core/tests/cassettes/record_cassette.py`: embedded proxy and scenario driver; Responses, Messages and Conversation Items modes |
| Conversations example | `crates/agentic-server-core/tests/cassettes/record_conversations_api_cassettes.sh` calls the Python recorder with `--mode items --items-scenario ...` |
| Conversation YAMLs | `crates/agentic-server-core/tests/cassettes/conversations/conversations-continuation-openai.yaml` and `conversations-continuation-gateway.yaml`; also `*-stream-*`, pagination, branch, deletion and edge-case pairs |
| Conversation replay | `crates/agentic-server-core/tests/conversations_api_cassette_test.rs` compares captures and replays requests through gateway handlers |
| Messages capture | `crates/agentic-server-core/tests/cassettes/record_messages_cassettes.sh` and `messages/` YAMLs capture upstream Messages traffic; inspect the provider used before claiming Anthropic parity |
| Messages tool-choice workflow | `crates/agentic-server-core/tests/cassettes/record_messages_tool_choice.py`; see the cassette README for provider comparisons and command options |
| WebSocket/multi-agent | `websocket_recorder.py`, `record_multi_agent_cassettes.sh`, `record_multi_turn_steering_cassettes.sh` under the same cassette directory; read scenario READMEs and transport-specific options |

The YAMLs are recorded output, not handwritten expected-response templates. For the Conversation Items workflow,
`turns` is flat and `tN` identifies each HTTP request. Preserve status codes, bodies, SSE sequence and ID relationships.

Inspect the shared Python CLI without sending a request:

```bash
uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  python crates/agentic-server-core/tests/cassettes/record_cassette.py --help
```

Example: record the existing Conversation Items continuation scenario for both OpenAI and a running gateway. Set
`OPENAI_API_KEY`, `OPENAI_MODEL`, `GATEWAY_MODEL`, and `GATEWAY_URL` for the intended environment first; never print the
key. This command makes live API calls and replaces the scenario's two YAML files, so inspect the current diff and
use the task's authorized recording scope. Do not run it merely to validate this skill.

```bash
CONVERSATIONS_RECORD_SET=all CONVERSATIONS_SCENARIO=continuation PYTHON_BIN=python \
  uv run --no-project --with click --with fastapi --with httpx --with uvicorn --with pyyaml \
  bash crates/agentic-server-core/tests/cassettes/record_conversations_api_cassettes.sh
```

Use `CONVERSATIONS_RECORD_SET=openai` or `gateway` to select one side. Use `continuation-stream`, `pagination`,
`pagination-stream`, or another scenario supported by the script rather than inventing a new flag. The shell script
contains the Python call, turn counts, output paths, and capture checks. For a new behavior, extend the nearest
scenario through that same recorder and describe the newly captured boundary.

Run the existing Conversation Items replay with missing-cassette failures enabled:

```bash
REQUIRE_CONVERSATIONS_CASSETTES=1 \
  cargo test -p agentic-server-core --test conversations_api_cassette_test
```

Compare the provider and gateway at the relevant boundary: admission/status/error envelopes, emitted items and events,
call/output identity, storage visibility, and a subsequent continuation where applicable. Assert actual upstream
requests and client-visible responses, not only helper outputs or successful parsing. Normalize nondeterministic IDs
and timestamps only through established comparison code while preserving their relationships. Do not delete failing
turns or normalize away the behavior being tested.

Check recorded files for credentials and private content using the existing redaction workflow before committing.
An intentional sanitized field is different from rewriting an observed provider response to make a test pass.
