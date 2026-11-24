# Direct vLLM integration

This unpublished library connects the Agentic API Responses executor to a headless
vLLM EngineCore using the shared Rust frontend. It owns model loading, per-round
request conversion, and native event conversion. Core owns request preparation,
history, tools, sessions, compaction, usage aggregation, and persistence.

The dependency points from this crate to core. The published core/server packages
do not depend on vLLM. The unpublished `agentic-server-native`
composition binary owns a headless engine using `vllm-managed-engine` and injects this backend into the shared gateway.
`agentic serve` and `agentic run codex` select it when `--upstream` is omitted. The source Python CLI's
`agentic-api run` delegates to `agentic serve`. Existing release wheels do not bundle the native binary.
See [configuration](../../docs/guides/configuration.md#direct-inference-in-source-builds) for `model.config` and setup.

## Model recipe and managed startup

`ModelConfig::load` reads TOML from the Agentic API home's `model.config`. The explicit model argument overrides
only the model field. Shared fields configure both engine startup and native frontend preparation; engine-only
arguments are forwarded to vLLM. Parser names are checked through vLLM's existing registry before spawning.
The runtime uses the physical model for assets and the optional served name for requests and catalog identity.

The composition binary registers signals before spawning, monitors engine exit during loading and serving, and
bounds startup and gateway drain. It closes the frontend and reaps the owned engine on startup failure, timeout,
signal, frontend health failure, or gateway failure. The shared gateway exposes the local model catalog, observes vLLM readiness (including paused engines), and rejects HTTP-only
inference endpoints. Storage, request preparation, tool routing, the loop, and response assembly remain shared.

## Composition

Construct `EngineCoreClientConfig` for an already running headless engine, then
load the runtime once at application startup:

```rust,no_run
use std::sync::Arc;
use agentic_core::executor::ExecutionContext;
use agentic_vllm::{EngineCoreClientConfig, LoadModelBackendsOptions, VllmBackend};

async fn attach(
    context: ExecutionContext,
    engine: EngineCoreClientConfig,
) -> agentic_vllm::Result<(ExecutionContext, Arc<VllmBackend>)> {
    let backend = Arc::new(VllmBackend::connect(
        engine,
        LoadModelBackendsOptions::default(),
    ).await?);
    let context = context.with_direct_inference(backend.clone());
    Ok((context, backend))
}
```

The engine configuration carries transport/handshake addresses, engine count,
model name, and startup timeout. Use the same model identity and frontend assets
as the engine. `LoadModelBackendsOptions` configures tokenizer/template resources;
`VllmBackend::from_chat` accepts an application-configured `ChatLlm` for explicit
parser selection. The shared frontend renders and tokenizes every round, including
compaction requests. It does not reload the model resources per request.

On shutdown, stop admission, drop executor contexts, and drain/drop active
generations. Recover the backend with `Arc::try_unwrap`, then call
`VllmBackend::shutdown`. The shared frontend owns engine abort and runtime tasks;
dropping an unfinished generation drops its engine stream. Core also cancels the
attempt token on admission cancellation, timeout, stream error, or drop.

## Current supported profile

- One configured model; text messages and plain reasoning history.
- Function tools and custom/shell tools lowered through existing core conversions.
  Existing namespace resolution and gateway tool orchestration remain in core.
- Temperature, top-p, output-token limit, ignore-EOS, reasoning effort, parallel
  tool choice, and cache salt. Omitted sampling values remain omitted for vLLM's
  own defaults. Metadata and prompt-cache hints remain orchestration hints.
- Parsed text, reasoning, and function-call events; completed and incomplete
  outcomes; engine usage, including cache writes, converted once into core `ResponseUsage`.
- The same executor handles JSON/SSE delivery, continuation, and compaction.

Unsupported model/input/options fail before engine submission: multimodal input,
opaque or summarized reasoning replay, structured output, requested logprobs,
non-disabled truncation, prompt-cache controls, non-default service tiers, upstream
extension fields, and additional reasoning/text controls. Parser and tool
support also depend on the loaded frontend/model. This profile does not establish
Anthropic Messages parity, a gRPC adapter, or distributed cache routing.

## Version and validation limits

The workspace pins vLLM to `f2f9edcac30d128bc090a3c032e4ba125716a641` and carries its
required PCRE2 partial-matching patch. Applications composing this git dependency
must retain that root Cargo patch; Cargo does not inherit dependency workspace
patches. The headless engine must use the corresponding compatible protocol.

Tests exercise real native event/request conversion through the shared executor
without a running engine. They cover reasoning, separate block/tool index spaces,
usage, terminal status, and unsupported requests. Core tests cover cancellation,
limits, client-tool continuation, and compaction. A live engine/GPU smoke test and
per-model prompt/parser conformance are still required for serving qualification.

The executor awaits client delivery before polling another native event and adds
no queue. Its event and retained-response limits apply to the shared pipeline.
The pinned upstream engine client uses an internal unbounded output queue, so
those limits do not establish an end-to-end memory bound under slow consumers.
