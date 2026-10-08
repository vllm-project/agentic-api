# Observability with OpenTelemetry

Agentic API can export traces and metrics to any OpenTelemetry Collector or
OTLP-compatible backend. Export is **off by default**: with no exporter
selected the gateway creates no provider, exporter thread, or network
connection, and only prints local logs as before.

This page covers HTTP and execution traces, upstream context propagation,
standard HTTP request metrics, trace-correlated local logs, and the export
lifecycle from [#279](https://github.com/vllm-project/agentic-api/issues/279).
Gateway-specific metrics remain a later phase.

## Enable export

Select an exporter per signal with the standard variables. Both default to
`none`; the OpenTelemetry specification defaults them to `otlp`, and this
gateway deliberately deviates so that telemetry stays opt-in.

```bash
export OTEL_TRACES_EXPORTER=otlp
export OTEL_METRICS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318
export OTEL_SERVICE_NAME=agentic-api
agentic-server
```

The only supported transport is OTLP over **HTTP/protobuf**. Setting
`OTEL_EXPORTER_OTLP_PROTOCOL` (or a signal-specific variant) to `grpc` or
`http/json` is a startup error rather than a silent fallback: the gRPC
exporter depends on `tonic 0.14`, whose minimum Rust version (1.88) is above
this repository's MSRV.

`OTEL_SDK_DISABLED=true` overrides every other variable and disables telemetry.

## Configuration reference

Variables validated by the gateway (invalid values fail startup):

| Variable | Values | Default |
| --- | --- | --- |
| `OTEL_SDK_DISABLED` | `true` \| `false` | `false` |
| `OTEL_TRACES_EXPORTER` | `none` \| `otlp` | `none` |
| `OTEL_METRICS_EXPORTER` | `none` \| `otlp` | `none` |
| `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`, `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | `http/protobuf` | `http/protobuf` |
| `OTEL_EXPORTER_OTLP_COMPRESSION`, `OTEL_EXPORTER_OTLP_TRACES_COMPRESSION`, `OTEL_EXPORTER_OTLP_METRICS_COMPRESSION` | `gzip`, or unset for no compression (`zstd` and the literal `none` are rejected) | unset |
| `OTEL_SERVICE_NAME` | any non-empty string | `agentic-api` |

Variables read directly by the OpenTelemetry SDK and OTLP exporter, with the
standard precedence *signal-specific variable → generic variable → default*:

| Variable | Purpose | Default |
| --- | --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` (`_TRACES_`, `_METRICS_`) | Collector base URL; `/v1/traces` and `/v1/metrics` are appended to the generic endpoint | `http://localhost:4318` |
| `OTEL_EXPORTER_OTLP_HEADERS` (`_TRACES_`, `_METRICS_`) | `key=value,...` headers, for example vendor authentication | none |
| `OTEL_EXPORTER_OTLP_TIMEOUT` (`_TRACES_`, `_METRICS_`) | Per-export request timeout in milliseconds; the effective bound on every export call | `10000` |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra `key=value` resource attributes | none |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | Sampler; see below | `parentbased_always_on` |
| `OTEL_BSP_MAX_QUEUE_SIZE`, `OTEL_BSP_SCHEDULE_DELAY`, `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` | Span batch processor | `2048`, `5000`, `512` |
| `OTEL_METRIC_EXPORT_INTERVAL` | Periodic metric reader interval in milliseconds | `60000` |
| `OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT`, `OTEL_SPAN_EVENT_COUNT_LIMIT`, `OTEL_SPAN_LINK_COUNT_LIMIT` | Per-span record limits | `128` |

The exported resource always carries `service.name` and `service.version`
(the gateway crate version); `OTEL_RESOURCE_ATTRIBUTES` is merged in.

`OTEL_BSP_EXPORT_TIMEOUT` and `OTEL_METRIC_EXPORT_TIMEOUT` are accepted by the
SDK but not applied by the processors this gateway uses (SDK 0.32); set
`OTEL_EXPORTER_OTLP_TIMEOUT` instead. The shutdown deadlines below apply on
top of it.

## What is exported

### Spans

Every HTTP request produces one `http.server.request` span, named
`{method} {route}` (for example `POST /v1/responses`) or just the method when
no route matched. A valid W3C `traceparent` header makes it a child of the
caller's span (and the caller's sampling decision is honoured); an absent or
invalid header starts a new trace. The span stays open until the response
body has been fully sent or the client disconnected, so streaming responses
are covered end to end. Attributes follow the HTTP semantic conventions and
are deliberately bounded:

| Attribute | Value |
| --- | --- |
| `http.request.method` | A well-known method, or `_OTHER` |
| `http.route` | The matched route template; absent for unmatched paths |
| `http.response.status_code` | Integer status |
| `url.scheme` | Always `http`: the gateway listens on plain HTTP and TLS, if any, terminates in front of it. Never copied from the request target |
| `network.protocol.version` | `1.1`, `2`, ... |
| `error.type` | Only on outcomes the middleware itself observes: `handler_panic`, `response_body`, or `cancelled` |

Query strings, request or response headers, client addresses, request and
response bodies, and upstream error bodies are never recorded. Spans are
marked with error status on a 5xx response, when a handler panics, or when
the response body fails mid-stream. A request dropped before any response
existed — a timeout around the handler, the client disconnecting, or the
runtime shutting down mid-request — carries `error.type=cancelled` with no
status code and is **not** marked as an error. A `2xx` on a streaming
response does **not** by itself mean the execution succeeded — that is what
the execution span below records.

WebSocket upgrades on `/v1/responses` produce a span for the upgrade request
only; the session and individual executions have separate traces described below.

#### Execution span

Every HTTP request the executor runs — Responses or Messages, streaming or not —
produces one `agentic.execute` span as a child of the HTTP server span. It
opens when the executor accepts the request and closes when the response
payload has been handed back or, for a stream, when the last frame has been
yielded or the client dropped the stream. Work done while streaming (later
inference rounds, persistence after the last round) is attributed to it.

| Attribute | Value |
| --- | --- |
| `agentic.api` | `responses` \| `messages` |
| `agentic.route` | `executor` \| `proxy` |
| `agentic.stream` | Whether the client asked for a stream |
| `agentic.queue.wait` | WebSocket only: seconds between admission and dispatch, or discard while queued |
| `agentic.execution.outcome` | `completed` \| `incomplete` \| `failed` \| `cancelled` |
| `agentic.delivery.outcome` | `delivered` \| `disconnected` \| `not_started` |
| `error.type` | On `failed` only, a bounded category derived from the error's *kind*, never its message: `storage`, `persistence`, `conversation_locked`, `upstream_status`, `upstream_transport`, `upstream_error`, `network`, `parse`, `stream`, `not_found`, `invalid_request`, `payload_too_large`, `resource_limit`, `round_budget`, `conflict`, `compaction`, `tool`, or `panic` |

Execution and delivery are recorded separately because they answer different
questions:

- **`execution.outcome`** is what the executor concluded. `completed` and
  `incomplete` (a Responses `incomplete` status or a Messages `max_tokens`
  stop) are terminal results; `failed` marks the span with error status; and
  `cancelled` means the executor was stopped before a terminal state — the
  stream was dropped or the task was aborted at shutdown — and is not an
  error.
- **`delivery.outcome`** is whether the transport was handed everything the
  execution produced: the payload, or the terminal streamed frame
  (`delivered`); the stream was dropped before that frame (`disconnected`);
  or the execution failed before there was anything to send (`not_started`).

So an SSE `error` frame under an HTTP 200 is `execution=failed`,
`delivery=delivered`; a client that leaves mid-stream is
`execution=cancelled`, `delivery=disconnected`; and a Messages upstream error
body, which the gateway relays verbatim, is `execution=failed`,
`error.type=upstream_error` even though the handler never saw an `Err`.

No request, response, or conversation identifier, model name, prompt, tool
argument, or error message appears on the span.

#### Execution stages and upstream propagation

Stages are children of `agentic.execute`, including work resumed from a
stream or spawned executor task. A typical Responses tool loop has two
`agentic.inference_round` children with `agentic.tool.execute` between them;
each round contains its upstream `http.client.request`.

| Span | Allowed stage attributes |
| --- | --- |
| `agentic.rehydrate` | `agentic.rehydrate.source`: `none`, `previous_response`, or `conversation` |
| `agentic.inference_round` | `agentic.inference.round`: zero-based round index |
| `agentic.tool.execute` | `agentic.tool.type`: the gateway's tool category (`function`, `tool_search`, `custom`, `shell`, `codex_namespace`, `mcp`, `web_search`, `file_search`, or `code_interpreter`), never the tool name |
| `agentic.compaction` | `agentic.compaction.operation`: `summarize`; `agentic.compaction.trigger`: `context_management`, `input_item`, or `explicit` |
| `agentic.persist` | `agentic.persist.destination`: `response` or `conversation` |

Rehydration, persistence, compaction, and gateway tool stages describe the
Responses executor. Inference rounds and HTTP client spans also cover the
Messages executor. Explicit `/v1/responses/compact` requests emit compaction,
rehydration, and persistence stages under their HTTP span, without an
`agentic.execute` wrapper. Compaction's summarization HTTP call is a child
of its compaction span.

The `http.client.request` span has client kind and only
`http.request.method=POST`, `server.address` (host only), `server.port`,
`http.response.status_code`, and, on failure, `error.type` (`timeout`,
`transport`, or `upstream_status`). It measures sending the request and
receiving headers, including reading an HTTP error body; streamed body
consumption continues inside the inference round. It never records
`url.full`, a URL path/query, headers, or an error message.

Both upstream API adapters replace outgoing `traceparent` and `tracestate`
with the active HTTP client span's W3C context. These headers carry context,
not span attributes; inherited trace state may travel with that context.

#### WebSocket sessions

Each socket has an independent root `agentic.websocket.session` span. Each
accepted `response.create` starts a new trace whose `agentic.execute` root
is **linked** to that session, not parented on it or on the HTTP upgrade.
This includes local `generate:false` requests. Invalid or rejected frames
do not create execution spans.

Pipelined requests start their spans on admission. `agentic.queue.wait`
records queue time in seconds at dispatch, or when an undispatched request
is discarded. Disconnecting cancels active and queued executions that have
not reached a terminal outcome. The session span has no application
attributes; links contain only trace context.

#### Raw proxy routes

Raw Responses and Messages proxy routes produce a thin `agentic.execute`
with `agentic.route=proxy`, parented on the HTTP server span. They replace
the caller's outbound trace headers with that execution span's context
without changing the request or response bytes.

Proxy outcomes describe **transport**, not model execution: a non-success
HTTP status is `failed` with `error.type=upstream_status`; successfully
forwarding the body is `completed`/`delivered`. Streaming completion is
detected at EOF or when the declared content length is exhausted. An early
drop is `cancelled`/`disconnected`; a body read failure is
`failed`/`disconnected` with `error.type=network`. The proxy does not parse
SSE terminal events, so an upstream semantic error inside an HTTP 200 body
does not turn the proxy span into a failed execution.

#### Attribute privacy contract

Export tests enforce per-span attribute allow-lists and bounded enum values
across real executor, WebSocket, and proxy traces. They scan attribute values
against fixture prompts, tool arguments, credentials (`Authorization` and
`x-api-key`), URL queries, summaries, and upstream error bodies. No such
content, tool name, model name, or application identifier belongs in span
attributes. Log events are not exported as span events, and error status
descriptions are empty. Operator-configured resource attributes and W3C
trace context are separate from this application-attribute contract.

### Metrics

| Instrument | Type | Attributes |
| --- | --- | --- |
| `http.server.request.duration` (seconds) | histogram | `http.request.method`, `http.route`, `http.response.status_code`, `url.scheme` |
| `http.server.active_requests` | up-down counter | `http.request.method`, `url.scheme` |

Duration boundaries are the semantic-convention set extended with `30`, `60`,
`120`, and `300` seconds because streamed inference responses routinely run
for minutes. Both instruments are recorded exactly once per request from a
guard attached to the response body, so a disconnecting client cannot leave
the active-request count drifting. Metrics are recorded regardless of the
trace sampling decision.

## Sampling

The default sampler is `parentbased_always_on`: HTTP traces honour a caller's
sampling decision carried in `traceparent`; requests without a parent are
sampled. WebSocket session and execution traces are independent roots, so
their sampling decisions come from the root sampler, not their links. For
high request volumes use ratio sampling:

```bash
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
```

Sampling affects only which spans are exported. Metrics and local logs are
unaffected.

## Local logs and correlation

Local log output is unchanged and still filtered by `RUST_LOG`. When traces
are exported, every log line emitted inside an exported span ends with
`trace_id=<32 hex> span_id=<16 hex>`, so a line can be matched to the trace in
your backend:

```text
2026-09-18T10:12:41.301Z  INFO http.server.request{...}: agentic_server::handler: routing HTTP responses request route=executor trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0ba902b7
```

Two independent filters are in play:

- `RUST_LOG` (default
  `agentic_server=info,agentic_core=info,opentelemetry_sdk=warn,opentelemetry-otlp=warn`)
  decides what is printed locally. It never changes what is exported.
- The export bridge only forwards spans at `INFO` and above whose target is
  one of this repository's crates (`agentic_server`, `agentic_core`,
  `agentic_praxis`, `agentic_llm_d`). Spans declared by dependencies are never
  exported, it never forwards log events, and exporting cannot feed back into
  itself.

The default `RUST_LOG` includes the SDK and exporter at `WARN` on purpose:
queue overflow and export failures are reported there, so a Collector that is
down is visible in the gateway's own logs.

Correlated lines are written without ANSI colour.

## Buffering, overflow, and shutdown

- Spans are batched by the SDK's batch processor on a dedicated thread. When
  the queue (`OTEL_BSP_MAX_QUEUE_SIZE`) is full, new spans are dropped: a
  warning is logged when dropping starts and the dropped count is reported at
  shutdown. Requests are never delayed or failed by export.
- Metrics are collected by a periodic reader on its own thread.
- A slow or unreachable Collector only affects the exporter threads: each
  export request is bounded by `OTEL_EXPORTER_OTLP_TIMEOUT`.
- On shutdown the gateway first drains in-flight requests (8 s budget), then
  stops the runtime with a 1 s deadline — any request still streaming past
  the drain budget is dropped here, which records its final span and metrics
  — and only then flushes and shuts down the telemetry providers with a 3 s
  deadline. A hung Collector therefore cannot hold the process beyond roughly
  12 s, well inside the 30 s termination grace period used by the Kubernetes
  manifests. Spans still buffered when the deadline passes are lost.

## Kubernetes

Add the variables to the gateway ConfigMap (see
[`deploy/kubernetes/configmap.yaml`](https://github.com/vllm-project/agentic-api/blob/main/deploy/kubernetes/configmap.yaml)),
pointing at a Collector Service or DaemonSet in the cluster:

```yaml
data:
  OTEL_TRACES_EXPORTER: "otlp"
  OTEL_METRICS_EXPORTER: "otlp"
  OTEL_EXPORTER_OTLP_ENDPOINT: "http://otel-collector.observability.svc:4318"
  OTEL_SERVICE_NAME: "agentic-api"
  OTEL_RESOURCE_ATTRIBUTES: "deployment.environment=staging"
```

The shipped `NetworkPolicy` restricts ingress only, so no egress rule is
needed for the Collector.
