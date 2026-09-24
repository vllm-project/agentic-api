# Embedded code interpreter design

> **Status:** Implemented as an opt-in feature.

## Scope

The Responses API accepts a gateway-executed built-in tool that runs Python in Eryx 0.8.x. Availability has two independent gates: the server binary must be compiled with the `embedded-code-interpreter` Cargo feature, and the operator must enable the executor with `code_interpreter.enabled = true` or `AGENTIC_CODE_INTERPRETER_ENABLED=true`. A request that declares the tool is rejected before upstream inference if either gate is absent or the executor did not pass startup readiness checks.

The public declaration is:

```json
{"type":"code_interpreter","execution":"gateway"}
```

The explicit `execution: "gateway"` selector is required. Omitting it, selecting another execution location, or including unknown fields is rejected instead of being silently ignored.

For model inference, the gateway normalizes this declaration to a strict function named `code_interpreter` with one required string argument, `code`, and no additional properties. The model must use `print()` to produce returned text. Implicit expression display, structured result variables, persistent sessions, file or artifact exchange, networking, callbacks, and client-selected containers are not exposed by this integration. Each call constructs a fresh sandbox.

## Integration

`CodeInterpreterHandler` owns declaration validation and model-facing normalization through the existing `ToolHandler` contract. In feature-enabled builds, `EryxCodeInterpreterExecutor` implements both `ToolHandler` and `GatewayExecutor` so the same component also supplies execution and public output projection. `GatewayExecutors::from_config` creates it only when the operator setting is enabled. Construction validates the limits and temporary directory, then builds a sandbox once to prove that the embedded runtime can initialize. An initialization failure prevents server startup.

`ToolRegistry` binds a ready executor with the same typed `GatewayBinding` used by other gateway-executed built-in tools. Calls then use the existing `GatewayScheduler` and multi-round tool loop; the integration does not add a second parser, scheduler, transport path, or streaming state machine.

For each call, the executor validates the JSON argument shape and UTF-8 source byte length before constructing a guest. It configures Eryx `ResourceLimits` for execution wall time, fuel, and maximum guest memory. One process-wide semaphore admits at most `min(max_concurrent_guests, max_aggregate_guest_memory_bytes / max_guest_memory_bytes)` calls. Admission does not wait: when no permit is available, the call fails with a capacity error.

An independently spawned Tokio task owns the permit and supervises one Eryx execution. The requesting future waits for that task through a one-shot channel. If the requesting future is dropped, its drop guard cancels the Eryx handle (or records cancellation until the handle exists); the supervisor continues waiting for Eryx to terminate and only then releases the permit. Per-call sandbox construction, which is synchronous, runs on Tokio's blocking pool.

Completed, failed, and resource-incomplete executions produce a `code_interpreter_call` output item. Non-empty retained stdout and stderr become separate `logs` entries. Initialization errors are logged with their operator-actionable detail and mapped to a fixed public diagnostic. Unexpected runtime failures also return a fixed public message rather than exposing the underlying Eryx error.

For a gateway-executed call, streaming responses synthesize this OpenAI Responses lifecycle around the public item: `response.output_item.added`, `response.code_interpreter_call.in_progress`, `response.code_interpreter_call_code.delta`, `response.code_interpreter_call_code.done`, `response.code_interpreter_call.interpreting`, `response.code_interpreter_call.completed`, and `response.output_item.done`. The gateway currently emits one code-delta event containing the complete source. The shared gateway stream accumulator assigns the output index and contiguous sequence numbers used by both HTTP/SSE and WebSocket delivery.

The model sees the normalized declaration as a function and therefore emits a canonical `function_call` lifecycle. After the translation dispatcher resolves that function name to a gateway-executed binding, it suppresses those canonical upstream frames; the gateway-generated `code_interpreter_call` lifecycle is public instead. If an upstream provider emits a native `code_interpreter_call`, typed ingestion validates and assembles it before the dispatcher sends its frames through the ordinary wire-restoration path without suppressing them. In strict mode, ingestion rejects an item-ID or item-kind conflict at one output index. In lenient mode, it may ignore a mismatched intermediate update, but still rejects a contradictory item opening or authoritative completion.

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

For example, `max_fuel` maps to `AGENTIC_CODE_INTERPRETER_MAX_FUEL`, and `execution_wall_time_seconds` maps to `AGENTIC_CODE_INTERPRETER_EXECUTION_WALL_TIME_SECONDS`. Fuel is WebAssembly work metering: Eryx stops execution when the configured fuel is exhausted even if wall time remains. All numeric limits must be positive, and one `max_guest_memory_bytes` charge must fit within the aggregate admission budget. The aggregate value is admission accounting; it does not measure the process's actual resident memory.

The feature is not part of the repository's default build. A user-built opt-in binary obtains its platform-matched artifact through the setup step above. This repository does not yet have an automated release pipeline that provisions and verifies that artifact, so prebuilt feature-enabled releases would additionally need provenance and checksum verification in their packaging workflow.

The locked Eryx version is 0.8.0, which declares `rust-version = "1.98.1"`. The checked-in development toolchain is therefore pinned to Rust 1.98.1 so feature builds pass Cargo's version check. This toolchain pin is distinct from the project's Rust 1.85 MSRV policy for default-feature builds.

At runtime, set `TMPDIR` to a dedicated operator-owned directory. On Unix it must have mode `0700`:

```console
install -d -m 700 "$HOME/.agentic-api/tmp"
TMPDIR="$HOME/.agentic-api/tmp" ./target/release/agentic-server
```

The server resolves `std::env::temp_dir()` at executor construction. It creates the directory if absent and, on Unix, requires its permission bits to be exactly `0700`. Eryx extracts and loads its embedded assets below that directory in `eryx-embedded`; a feature-enabled, operator-enabled server fails startup if the directory check or the eager sandbox initialization fails.

## Known containment limitation

Eryx 0.8.0 accumulates complete stdout/stderr in `ExecuteResult`, uses an unbounded internal output channel, and inherits raw WASI stdout/stderr. The gateway's bounded `OutputHandler` cancels normal Python output when its configured limit is crossed and returns only the bounded prefix, but this is not a hard host-memory/output bound: raw descriptor writes such as `os.write` can bypass the handler, and Eryx may allocate output before cancellation is observed.

Consequently both the Cargo feature and operator setting remain disabled by default. This repository should not ship a prebuilt feature-enabled production artifact until its packaging verifies the runtime artifact and Eryx provides an API (or the project carries a reviewed downstream patch) that bounds output at the WASI/WIT production boundary without inheriting host stdout/stderr. The current wall-time, fuel, guest-memory, admission, and retained-output limits do not remove this host-output and host-memory limitation; operators who build and enable the feature accept it explicitly.

## Verification

Default-feature tests cover the closed request shape, typed normalization, name collisions, fail-closed HTTP and WebSocket declaration handling, accumulator support for native upstream items, and the gateway-generated lifecycle. Recorder-generated cassettes characterize the public non-streaming, HTTP/SSE, and WebSocket shapes against an OpenAI reference and a gateway execution.

The dedicated `Embedded code interpreter` CI job prepares the runtime with `scripts/setup-eryx-runtime.sh`, creates a fresh private `TMPDIR`, and runs feature-enabled linting and tests for both server crates. The real-Eryx test executes a successful program, a Python exception, filesystem isolation probes, and output overflow through `eryx::Sandbox`. The setup-script test itself uses a fake precompiler to verify version matching and the `cargo install`/`cargo binstall` branches without downloading a runtime.
