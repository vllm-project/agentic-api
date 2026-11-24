# Embedded code interpreter design

> **Status:** Implemented as an opt-in feature.

## Scope

The Responses API accepts a gateway-executed built-in tool that runs Python in Eryx 0.8.x. Availability has two independent gates: the server binary must be compiled with the `embedded-code-interpreter` Cargo feature, and the operator must enable the executor with `code_interpreter.enabled = true` or `AGENTIC_CODE_INTERPRETER_ENABLED=true`. A request that declares the tool is rejected before upstream inference if either gate is absent or the executor did not pass startup readiness checks.

The gateway accepts the OpenAI Responses API declaration used by SDK clients:

```json
{"type":"code_interpreter","container":{"type":"auto"}}
```

The auto container is required. The `execution` field, explicit container IDs, `file_ids`, `memory_limit`, and unknown fields are rejected. The public response and stored metadata retain the accepted declaration.

The `container: {"type":"auto"}` selector chooses gateway execution, but the gateway creates a fresh sandbox for every call. OpenAI's auto mode may reuse a container from prior context; this integration does not provide container reuse. The container selector does not grant the client control over the gateway runtime.

For model inference, the gateway normalizes the declaration to a strict function named `code_interpreter` with one required string argument, `code`, and no additional properties. The model must use `print()` to produce returned text. Implicit expression display, structured result variables, persistent sessions, file or artifact exchange, networking, callbacks, and client-selected containers are not exposed by this integration.

## Integration

The public `CodeInterpreterHandler` owns declaration validation and model-facing normalization through the existing `ToolHandler` contract in every build. Supported request flows check runtime availability before normalization. In feature-enabled builds, the private, provider-backed `CodeInterpreterExecutor` implements `ToolHandler` and `GatewayExecutor`; it owns shared admission, cancellation supervision, and public output projection. The provider contract and common executor compile in every build; a `CodeInterpreterProvider` supplies execution, startup readiness, and capacity. The current `EryxProvider` is compiled only with `embedded-code-interpreter`. The existing feature gate and operator enablement select it; a public provider selector is deferred until another backend exists. `GatewayExecutors::from_config` creates the private executor only when the operator setting is enabled. Construction validates the limits, private temporary directory, delegated cgroup, and worker executable, then probes Eryx initialization inside a limited worker. A failed probe prevents server startup.

`ToolRegistry` binds a ready executor with the same typed `GatewayBinding` used by other gateway-executed built-in tools. Calls then use the existing `GatewayScheduler` and multi-round tool loop; the integration does not add a second parser, scheduler, transport path, or streaming state machine.

For each call, the handler validates the JSON argument shape and UTF-8 source byte length before constructing a guest. The Eryx provider configures `ResourceLimits` for execution wall time, fuel, and maximum guest memory. A separate Linux cgroup v2 `memory.max` limits memory charged after worker attachment, including Eryx initialization and host output allocations. The worker cgroup sets `memory.swap.max` to zero so swap cannot extend that limit. One process-wide semaphore admits at most the configured concurrency and the number of per-call guest and worker reservations that fit their respective aggregate budgets. Admission does not wait: when no permit is available, the call fails with a capacity error.

An independently spawned Tokio task owns the permit and supervises one child process. The requesting future waits for that task through a one-shot channel. If the requesting future is dropped, its drop guard requests cancellation; the supervisor terminates and reaps the worker before releasing the permit. The worker initializes Eryx and executes one guest after the parent has attached it to a limited cgroup and verified that limit. The worker's stdout and stderr go to `/dev/null`; a private Unix socket carries bounded control messages and retained output.

Completed, failed, and resource-incomplete executions produce a `code_interpreter_call` output item. The common handler enforces final retained-output byte limits for every provider. Non-empty retained stdout and stderr from gateway Eryx execution become separate `logs` entries. Native upstream `code_interpreter_call` items may also contain typed `image` outputs; the Eryx provider does not generate image outputs. Initialization, worker crash, and unexpected runtime failures use fixed diagnostics; raw worker stderr and Eryx details are discarded so guest output cannot enter gateway logs through that channel.

For a gateway-executed call, streaming responses synthesize this OpenAI Responses lifecycle around the public item: `response.output_item.added`, `response.code_interpreter_call.in_progress`, `response.code_interpreter_call_code.delta`, `response.code_interpreter_call_code.done`, `response.code_interpreter_call.interpreting`, `response.code_interpreter_call.completed`, and `response.output_item.done`. The gateway currently emits one code-delta event containing the complete source. The shared gateway stream accumulator assigns the output index and contiguous sequence numbers used by both HTTP/SSE and WebSocket delivery.

The model sees the normalized declaration as a function and therefore emits a canonical `function_call` lifecycle. After the translation dispatcher resolves that function name to a gateway-executed binding, it suppresses those canonical upstream frames; the gateway-generated `code_interpreter_call` lifecycle is public instead. If an upstream provider emits a native `code_interpreter_call`, typed ingestion validates and assembles it before the dispatcher sends its frames through the ordinary wire-restoration path without suppressing them. In strict mode, ingestion rejects an item-ID or item-kind conflict at one output index. In lenient mode, it may ignore a mismatched intermediate update, but still rejects a contradictory item opening or authoritative completion.

Native upstream `code_interpreter_call` items remain typed continuation input across inference rounds and stored response or conversation history. Gateway-generated public calls carry private origin metadata in storage and are omitted from continuation input because their model-facing `function_call` and `function_call_output` are retained separately. The origin metadata is not exposed in the public item.

## HTTP multi-agent execution

Stored HTTP multi-agent requests use the same interpreter executor in each agent's
tool registry. A model call is normalized as `code_interpreter({"code": "..."})`;
the gateway executes it and supplies the canonical function call output to that
agent. The client sees an attributed `code_interpreter_call` and, for SSE, its
normal lifecycle with response-wide output indexes. Interpreter execution does not
require a client continuation. A local shell call still does.

All agents share the ready executor and its guest limits. Subagent slots do
not create independent interpreter capacity budgets. Startup gates, fresh
sandbox semantics, failure handling and cancellation are unchanged. Persisted agent
histories retain completed canonical calls and outputs across client continuations.

The `code-interpreter` suite in `record_multi_agent_cassettes.sh` asks two subagents
to find four distinct popular LeetCode problems online, two per agent, implement them in
Python, execute example inputs, and summarize the algorithms, sources and actual outputs.
Web search and code interpreter are declared, with `parallel_tool_calls: true`. Parallel calls
are permitted, not forced; an agent may execute both algorithms in one Python call.
Record OpenAI first, then the gateway with the runtime below enabled:

```bash
MULTI_AGENT_RECORD_SET=openai MULTI_AGENT_SUITE=code-interpreter \
  MULTI_AGENT_STREAM_MODE=both MAX_CONCURRENT_SUBAGENTS=2 \
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh

MULTI_AGENT_RECORD_SET=gateway MULTI_AGENT_SUITE=code-interpreter \
  MULTI_AGENT_STREAM_MODE=both MAX_CONCURRENT_SUBAGENTS=2 \
  GATEWAY_URL=http://localhost:9000 GATEWAY_MODEL=your-served-model \
  bash crates/agentic-server-core/tests/cassettes/record_multi_agent_cassettes.sh
```

The recorder supplies no simulated tool outputs. Code interpreter outputs come
from actual service execution. The algorithm prompt is in
`multi_agent/prompts.txt`; the tools are in `multi_agent/code_interpreter_tools.json`.

The Rust cassette integration tests compare both transports with the OpenAI
references. They verify two child owners, web search and completed Python calls,
gateway execution logs, and the attributed streaming lifecycle and code deltas.
OpenAI's recorded `outputs: null` is allowed; model text and call counts need not
match. These checks need no interpreter runtime or live service:

```bash
cargo test -p agentic-server-core --test multi_agent_contract_test -- --test-threads=2
```

## Runtime provisioning

### Opt-in source build

Run the setup script and feature-enabled build from the repository root:

```console
./scripts/setup-eryx-runtime.sh
cargo build --release -p agentic-server --features embedded-code-interpreter
```

The setup script prepares Eryx's build-time runtime artifact; it does not build the server or enable the executor at runtime. It performs these steps:

1. Reads the exact locked Eryx version from `Cargo.lock`.
2. Uses `ERYX_PRECOMPILE_BIN` when explicitly set, otherwise searches `PATH` for `eryx-precompile`.
3. Requires an explicitly selected binary to match the locked version. When an automatically discovered binary is absent or has another version, installs the exact locked version with `cargo binstall` when available, otherwise with `cargo install`.
4. Runs the version-matched binary's `setup` command, which downloads and precompiles the host-platform runtime into Eryx's user cache.

By default, installation uses `CARGO_HOME` or `~/.cargo`. Set `ERYX_PRECOMPILE_INSTALL_ROOT` to choose another installation root. Preparing the artifact is only the build prerequisite; the Cargo feature and the separate operator setting described below are still required.

Eryx's build script locates `runtime.cwasm` in this order:

1. The explicit path in `ERYX_RUNTIME_CWASM`.
2. A `runtime-v<version>*.cwasm` artifact in `$XDG_CACHE_HOME/eryx` or `~/.cache/eryx`, as populated by `eryx-precompile setup`.
3. `../eryx-runtime/runtime.cwasm`, for development inside an Eryx workspace.

The feature-enabled Cargo build fails with setup instructions when no artifact is found. When Eryx finds one, its build script copies the artifact into Cargo's build output and `include_bytes!` embeds that copy in the server binary. `ERYX_RUNTIME_CWASM` therefore selects an existing build-time artifact; it does not build one, and the resulting server does not read that variable at runtime.

The compiled executor remains disabled until the operator adds this top-level table to `~/.agentic-api/config.toml`:

```toml
[code_interpreter]
enabled = true
```

`AGENTIC_CODE_INTERPRETER_ENABLED=true` is the higher-precedence environment equivalent. Environment values override the corresponding file values; absent values use the defaults below. Invalid environment values fail configuration construction. The Cargo feature and operator enablement are both required.

The remaining `[code_interpreter]` keys are operator-owned limits. Each has an environment override with the same name in uppercase and an `AGENTIC_CODE_INTERPRETER_` prefix.

| `config.toml` key | Default | Purpose |
| --- | ---: | --- |
| `max_source_bytes` | 65,536 | Maximum UTF-8 Python source size |
| `execution_wall_time_seconds` | 10 | Elapsed-time deadline after guest initialization |
| `max_fuel` | 10,000,000,000 | Wasmtime fuel budget for one execution; independent of wall time |
| `max_guest_memory_bytes` | 134,217,728 | Per-execution Eryx guest-memory limit and admission charge |
| `max_stdout_bytes` | 65,536 | Retained standard-output bytes |
| `max_stderr_bytes` | 65,536 | Retained standard-error bytes |
| `max_concurrent_guests` | 2 | Process-wide simultaneous execution limit |
| `max_aggregate_guest_memory_bytes` | 268,435,456 | Process-wide admission budget, divided into per-call charges |
| `max_worker_memory_bytes` | 1,073,741,824 | Hard cgroup v2 cap on one worker's post-attachment memory charges |
| `max_aggregate_worker_memory_bytes` | 2,147,483,648 | Admission budget for worker cgroup memory reservations |

For example, `max_fuel` maps to `AGENTIC_CODE_INTERPRETER_MAX_FUEL`, and `execution_wall_time_seconds` maps to `AGENTIC_CODE_INTERPRETER_EXECUTION_WALL_TIME_SECONDS`. Fuel is WebAssembly work metering: Eryx stops execution when the configured fuel is exhausted even if wall time remains. All numeric limits must be positive. One guest and one worker reservation must each fit within their respective aggregate admission budgets, and a worker memory limit must exceed its guest-memory limit. The aggregate values are admission accounting; post-attachment worker memory is constrained by each worker's `memory.max`, with `memory.swap.max` set to zero. The bounded worker protocol accepts at most 524,288 source bytes and 2,097,152 combined retained stdout/stderr bytes; configuration above either ceiling fails validation.

The feature is not part of the repository's default build. A user-built opt-in binary obtains its platform-matched artifact through the setup step above. This repository does not yet have an automated release pipeline that provisions and verifies that artifact, so prebuilt feature-enabled releases would additionally need provenance and checksum verification in their packaging workflow.

The locked Eryx version is 0.8.0, which declares `rust-version = "1.98.1"`. The checked-in development toolchain and the project's MSRV are therefore both Rust 1.98.1, so every feature builds on the supported toolchain.

At executor construction, an explicit `TMPDIR` selects the interpreter's temporary
directory. When it is unset, the gateway uses `<AGENTIC_API_HOME>/tmp`, or
`~/.agentic-api/tmp` when the application home is also unset. The directory is
created with mode `0700` if absent. Existing directories must already have private
permissions; symlinks are rejected, and Linux also verifies ownership and trusted
parent directories.

The resolved path is retained by the executor, used for worker control sockets,
and passed as `TMPDIR` to each isolated worker. The gateway does not modify its
process-wide environment. An enabled server also needs a delegated cgroup v2 parent
with `memory` and `pids` enabled for child groups, with the gateway already running
in a separate leaf. Startup fails if the directory, cgroup controls, worker
executable, or limited Eryx readiness probe is unavailable.

The server establishes this layout before telemetry or Tokio starts threads. It
reuses a prepared gateway leaf, or creates one inside a delegated scope occupied
only by the startup process. Otherwise, it re-executes itself through
`systemd-run --user --scope --property=Delegate=yes` once, preserving arguments,
environment, and standard streams. An internal retry marker prevents a delegation
loop. The private worker entry point bypasses this server startup path.

Run the feature-enabled binary or Cargo directly:

```console
cargo run --release -p agentic-server --bin agentic-server \
  --features embedded-code-interpreter -- --llm-api-base http://127.0.0.1:5050
```

A systemd user manager must be available for automatic delegation. A production
service can instead provide `Delegate=yes`; startup creates the gateway leaf
before starting threads. The service manager must terminate the delegated scope
as a unit if the gateway exits unexpectedly, so in-flight workers cannot outlive
it. Cgroup controls live in the kernel's `/sys/fs/cgroup` hierarchy; temporary
worker files live in the application home and require no additional metadata.
Missing delegation or controllers causes startup to fail without running
uncontained workers. The test wrapper remains available for test executables
that do not run through the server's startup path.

## Worker containment boundaries

Eryx 0.8.0 accumulates complete stdout/stderr in `ExecuteResult`, uses an unbounded internal output channel, and inherits raw WASI stdout/stderr. The bounded `OutputHandler` cancels normal Python output when its configured limit is crossed, but Eryx may allocate more output before cancellation takes effect. These allocations now occur after the worker joins its per-call cgroup, where `memory.max` limits their host memory consumption. Raw WASI descriptor writes, including `os.write(1, ...)` and `os.write(2, ...)`, go to `/dev/null` rather than the gateway's logs or control channel. They are not returned as execution logs.

The worker has a separate bounded control socket, a process-count limit, and a parent-supervised deadline. Its small trusted loader/startup allocations before cgroup attachment remain charged to the parent cgroup because [cgroup migration does not move existing memory charges](https://www.kernel.org/doc/html/v4.19/admin-guide/cgroup-v2.html#organize-once-and-control); Eryx initialization and guest code are blocked until attachment is verified. An OOM, timeout, disconnect, or cancellation ends the worker and releases its admission permit only after shutdown. These protections require a supported delegated Linux cgroup v2 hierarchy and are checked at startup. The worker still runs as the gateway's OS user and relies on Eryx's WASI filesystem and network restrictions for guest access. The Cargo feature and operator setting remain disabled by default. Prebuilt feature-enabled releases still require build-time runtime artifact provenance and verification.

## Verification

Default-feature tests cover the OpenAI request shape and rejection of unsupported container settings, typed normalization, name collisions, fail-closed HTTP and WebSocket declaration handling, accumulator support for native upstream items, and the gateway-generated lifecycle. Recorder-generated cassettes characterize the public non-streaming, HTTP/SSE, and WebSocket request declarations and response shapes against an OpenAI reference and a gateway execution. The gateway captures exercise the auto-container declaration through real Eryx execution.

The dedicated `Embedded code interpreter` CI job prepares the runtime with `scripts/setup-eryx-runtime.sh`, creates a fresh private `TMPDIR`, verifies delegated cgroup v2 support, and builds the real worker executable. Its feature-enabled core and server tests run inside a pre-start delegated scope. A live HTTP integration test sends a public code-interpreter declaration through tool normalization, a mock upstream function call, the isolated Eryx worker, and a second inference round carrying the tool output. The explicit ignored worker tests cover real Eryx execution, raw descriptor output containment, timeout recovery, active cancellation and process reaping. A separate page-touching allocator test uses the same cgroup creation and attachment path to verify kernel OOM enforcement; it is a cgroup test, not an Eryx OOM test. The setup-script test uses a fake precompiler to verify version matching and the `cargo install`/`cargo binstall` branches without downloading a runtime. CI execution on the hosted runner remains to be confirmed after this branch is pushed.
