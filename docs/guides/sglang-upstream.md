# Running Agentic API in front of SGLang

Point Agentic API at SGLang's `/v1/responses` endpoint using `--llm-api-base`.
The gateway manages stored responses and rehydrates item history for subsequent turns.

## Launch configuration

The conformance recorder targets SGLang **0.5.18** and `Qwen/Qwen3-8B`.
The model uses the `qwen3` reasoning parser and `qwen25` tool-call parser. See the upstream
[installation guide](https://docs.sglang.ai/get_started/install.html) and
[DGX Spark guide](https://lmsys.org/blog/2025-11-03-gpt-oss-on-nvidia-dgx-spark/).

```bash
docker run --name agentic-sglang --gpus all --shm-size 8g \
  -p 127.0.0.1:30000:30000 \
  -v "$HOME/.cache/huggingface:/root/.cache/huggingface" \
  lmsysorg/sglang@sha256:9e148f5ac788e856a06166bd6347a831831eb9fcfab4d1770874823a7c29a1a1 \
  python3 -m sglang.launch_server \
  --model-path Qwen/Qwen3-8B --host 0.0.0.0 --port 30000 \
  --revision b968826d9c46dd6066d109eabc6255188de91218 \
  --reasoning-parser qwen3 --tool-call-parser qwen25 \
  --mem-fraction-static 0.3 --context-length 8192 --cuda-graph-max-bs 4
```

Once `http://127.0.0.1:30000/health` succeeds, start the gateway:

```bash
cargo run -p agentic-server -- --llm-api-base http://127.0.0.1:30000
```

## Record and replay

The SGLang suite uses the same executor replay assertions as Dynamo: blocking and
streaming text, gateway-managed two-turn continuation, and a client-executed function
call. It compares each provider's output with its own captured traffic. The CI job
replays fixtures without a GPU or a running model.

From the repository root, install the recorder dependencies into a virtual environment
and run (Rust is also required for replay validation):

```bash
uv venv .venv
uv pip install --python .venv/bin/python click fastapi httpx uvicorn 'PyYAML==6.0.3'

PYTHON=.venv/bin/python SGLANG_URL=http://127.0.0.1:30000 \
  SGLANG_VERSION=0.5.18 MODEL=Qwen/Qwen3-8B \
  bash crates/agentic-server-core/tests/cassettes/record_sglang_cassettes.sh

cargo test -p agentic-server-core --test sglang_cassette_test
```

The script records through `record_cassette.py` into a temporary staging directory.
It builds the second turn from the captured first-turn assistant message, removes
request headers and unstable identifiers/timestamps, and adds provider provenance.
Structural validation and executor replay must pass before fixtures are copied into
the repository. Staging files remain available for diagnosing a failed refresh.
Review the resulting diff and commit intentional updates; never fabricate captured YAML.

Negative integer `sequence_number` values are treated as unspecified during event
normalization. The normal streaming delivery path assigns client sequence numbers;
text, function-call payloads, and terminal usage still pass through ingestion.

The initial recording set does not establish support for parallel function calls,
structured text, reasoning summaries, or upstream WebSocket transport. These are
listed as unverified in provenance. The broader live-engine matrix and scenario
expansion remain tracked in [issue #211](https://github.com/vllm-project/agentic-api/issues/211).

## GPT-OSS limitation in SGLang 0.5.18

A live GPT-OSS 20B probe completed blocking continuation, but streaming changed
reasoning item IDs/output indexes before completion and returned an empty terminal
output array. The gateway rejects this malformed lifecycle. Do not infer GPT-OSS
streaming support from the Qwen3 baseline.

GPT-OSS also requires Harmony vocabularies. A failed vocabulary download can disable
SGLang's optional Responses handler while `/health` still succeeds. Check startup
logs for `OpenAI Responses API ... disabled`; the upstream DGX Spark guide explains
mounting the vocabulary files with `TIKTOKEN_ENCODINGS_BASE`.
