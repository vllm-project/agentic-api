# Deploy agentic-api on Render

The repository includes a Render Blueprint that builds the production container, creates a managed PostgreSQL
database, and connects the gateway to an external OpenAI-compatible inference service. Because the gateway does not
authenticate inbound callers yet, the Blueprint creates a private service with no public URL in a network-isolated
production environment. The default Blueprint uses paid `starter` compute and `basic-256mb` PostgreSQL plans.
Network isolation requires a Pro workspace or higher; review current Render pricing before creating it.

## Create the Blueprint

1. In the Render Dashboard, choose **New > Blueprint** and connect this repository.
2. Select the repository's `render.yaml`.
3. Enter `LLM_API_BASE`, including the scheme and host of the external inference service.
4. Enter `OPENAI_API_KEY` when the inference service requires one. For an unauthenticated vLLM endpoint, leave it
   empty or remove the variable before creating the Blueprint.
5. Enter `false` for `SKIP_LLM_READY_CHECK` when the inference service exposes `/health`, or `true` when it does not.
   Render preserves this operator-supplied value on later Blueprint syncs.
6. Review the proposed resources and create the Blueprint.

Render builds the existing `Dockerfile` with BuildKit and starts its `docker-entrypoint.sh`. The service binds to
`0.0.0.0:9000`. Render does not assign it a public `onrender.com` URL, and the production environment blocks private
network traffic from other environments in the workspace. `DATABASE_URL` comes from the managed database's private
`connectionString`; no database credential is committed to the repository. The database blocks public connections
because its `ipAllowList` is empty.

The gateway applies its storage schema during startup. A failed database connection or schema initialization prevents
the process from listening, so Render does not promote that deploy.

## Configure readiness

Render private services support TCP health checks, so Render verifies that the gateway is listening on port `9000`
but does not call `/ready`. By default, the gateway still calls `<LLM_API_BASE>/health` before it starts listening;
failure to reach a healthy inference service therefore prevents the deploy from becoming ready. Configure the
authenticated edge to call `/ready` for upstream monitoring. That endpoint forwards the configured upstream bearer
credential when present and returns `503 Service Unavailable` when the inference service is unhealthy. It does not
check PostgreSQL; use the persistence smoke test below for that dependency.

Some hosted OpenAI-compatible providers do not expose `/health`. For those providers, set
`SKIP_LLM_READY_CHECK=true` during Blueprint creation. This skips startup polling only. Because `/ready` still calls
`/health`, monitor such providers with an authenticated `/v1/models` or small Responses API request instead.

The Blueprint gives Render 30 seconds to stop an old instance. The gateway uses the first eight seconds to drain HTTP
requests and WebSockets before closing remaining connections.

## Add an authenticated edge

The gateway does not authenticate inbound callers yet. `OPENAI_API_KEY` is an upstream credential and is not a client
password. Keep the gateway private and put a separate authenticated web service or another identity-aware proxy in
front of it. Move that edge into the same network-isolated `agentic-api` production environment so it can reach the
gateway; services outside the environment cannot bypass it over the private network. The edge must authenticate every
`/v1/*` HTTP and WebSocket request before forwarding it. Do not convert the gateway itself to a public `web` service
until it has an application identity boundary; otherwise anonymous callers could spend the upstream credential or
write stored state.

Keep every secret in Render environment variables or an environment group. Never place credentials in `render.yaml`,
Docker build arguments, or the repository.

## Verify the deployment

From a machine with `curl` and `jq` that can reach the authenticated edge, set the edge's public URL and the client
credential it requires:

```console
export AGENTIC_API_URL=https://agentic-api.example.com
export AGENTIC_API_AUTH='Authorization: Bearer replace-me'
curl --fail --header "$AGENTIC_API_AUTH" "$AGENTIC_API_URL/health"
curl --fail --header "$AGENTIC_API_AUTH" "$AGENTIC_API_URL/ready"
```

Exercise persistence with a stored response. Replace the model name with one served by the configured inference
backend:

```console
first_response_id=$(
  curl --fail --silent --show-error "$AGENTIC_API_URL/v1/responses" \
    --header "$AGENTIC_API_AUTH" \
    --header "Content-Type: application/json" \
    --data '{"model":"Qwen/Qwen3-30B-A3B-FP8","input":"Reply with READY","store":true}' |
    jq --exit-status --raw-output .id
)

curl --fail --silent --show-error "$AGENTIC_API_URL/v1/responses" \
  --header "$AGENTIC_API_AUTH" \
  --header "Content-Type: application/json" \
  --data "$(jq --null-input --arg id "$first_response_id" '{
    model: "Qwen/Qwen3-30B-A3B-FP8",
    input: "What word did you return?",
    previous_response_id: $id,
    store: true
  }')"
```

Redeploy the service, then repeat the continuation using the same previous response ID. A successful continuation
confirms that state survived the replacement of the gateway instance.

External WebSocket clients connect to the authenticated edge with `wss`; the edge can proxy to the private service
over Render's private network. Render can replace instances during deploys and does not guarantee that a reconnect
reaches the same instance, so clients should send keepalive pings and reconnect with backoff. Server-sent event
streams use the same authenticated HTTPS edge and need no additional Render process.

## Production considerations

- Keep the gateway, managed PostgreSQL, and inference service in nearby regions to reduce request and persistence
  latency.
- Configure PostgreSQL backups and retention in Render according to the application's recovery objectives.
- The Blueprint intentionally does not attach a persistent disk. Gateway state belongs in PostgreSQL, and Render's
  service filesystem is ephemeral.
- Render is the simpler single-service path. Use Kubernetes when the deployment needs custom ingress policy,
  independent migration jobs, advanced autoscaling, or cluster-level network controls.
