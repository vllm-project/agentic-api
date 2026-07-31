# Run agentic-api in a container

The production image contains only the Rust gateway and its runtime libraries. It does not contain Python, vLLM, GPU libraries, model weights, Cargo, or the Rust toolchain. Run inference and PostgreSQL as external services.

## Build the image

The multi-stage build pins its Rust and Debian bases by digest, uses BuildKit caches, and copies only `agentic-server` into the runtime stage. Dependabot proposes weekly digest updates so base-image changes remain explicit and reviewable.

```console
DOCKER_BUILDKIT=1 docker build \
  --build-arg OCI_CREATED="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --build-arg OCI_REVISION="$(git rev-parse HEAD)" \
  --build-arg OCI_VERSION="dev" \
  --tag agentic-api:dev \
  .
```

CI also records the workflow name and run URL in the vLLM-compatible image labels. Local builds use `local` as the pipeline label unless `OCI_BUILD_PIPELINE` is supplied as a build argument.

Pass `--build-arg CARGO_BUILD_JOBS=<n>` if the builder needs a different concurrency limit. For multi-platform publishing, use a Buildx builder with native workers where available; emulation is slower.

## Configure the gateway

The image starts `agentic-server` in standalone mode. At minimum, set `LLM_API_BASE` to an external OpenAI-compatible or vLLM endpoint. Production deployments should also set `DATABASE_URL` to an external PostgreSQL database.

| Variable | Default | Purpose |
| --- | --- | --- |
| `LLM_API_BASE` | none | Required upstream inference URL |
| `GATEWAY_HOST` | `0.0.0.0` | Listen address |
| `GATEWAY_PORT` | `9000` | Listen port |
| `DATABASE_URL` | `sqlite://./agentic_api.db` | SQLite or PostgreSQL persistence URL |
| `OPENAI_API_KEY` | none | Credential sent to the upstream service when the client does not supply one |
| `SKIP_LLM_READY_CHECK` | `false` | Skip the startup probe for providers without `/health` |
| `CORS_ALLOWED_ORIGINS` | none | Comma-separated browser origins |

The container entrypoint rejects percent-encoded SQLite paths because SQLx decodes them before opening the database. Use a literal filesystem path or PostgreSQL instead.

Do not put credentials into the image or Docker build arguments. Inject them at runtime through a secret manager.

```console
docker run --rm --name agentic-api \
  --publish 127.0.0.1:9000:9000 \
  --env LLM_API_BASE=https://vllm.example.com \
  --env DATABASE_URL=postgresql://agentic-api@postgres.example.com/agentic_api \
  --env OPENAI_API_KEY \
  agentic-api:dev
```

The gateway does not provide inbound client authentication. `OPENAI_API_KEY` is an upstream credential, not a password for callers, so keep the port bound to loopback unless an authenticated ingress or proxy protects it.

If the upstream is running on the Docker host, use `http://host.docker.internal:<port>` on Docker Desktop. On Linux, add `--add-host host.docker.internal:host-gateway`.

## Smoke test

The liveness probe reports whether the gateway process is serving traffic. Readiness also checks the upstream inference service.

```console
curl --fail http://127.0.0.1:9000/health
curl --fail http://127.0.0.1:9000/ready
```

The container CI workflow builds the image, verifies that build tools are absent, launches the gateway against a mock upstream, checks both probes, and exercises a stored Responses API request through SQLite persistence. HTTP streaming and WebSockets use the same gateway binary and exposed port; the image does not add a transport proxy.

On `SIGTERM`, the gateway stops accepting connections and gives in-flight requests up to eight seconds to drain before closing the remaining connections. Set an orchestrator termination grace period longer than eight seconds; the default 30-second Kubernetes grace period and the documented 10-second Docker stop timeout both satisfy this requirement.

## Kubernetes and OpenShift security context

The image defaults to UID `10001` and GID `0`. Its working directory is setgid and the entrypoint uses a group-cooperative umask, so new SQLite files remain writable when OpenShift replaces the UID while retaining the group-0 permission model. Do not set a fixed `runAsUser` when the cluster assigns arbitrary UIDs.

A volume mounted at `/var/lib/agentic-api` hides the ownership and mode stored in the image. For SQLite, configure the storage class or pod-level `fsGroup` so the mounted directory is writable by a supplemental group assigned to the container. The example below uses group 0 to match the image; if the cluster assigns a different permitted supplemental group, use that group and ensure the volume root is group-writable and setgid. PostgreSQL deployments do not need this pod-level filesystem setting.

Volumes initialized by an older image may contain SQLite files without group-write permission. Before rotating to an arbitrary UID, repair those volumes once as an administrator with `chmod -R g+rwX /var/lib/agentic-api`.

```yaml
spec:
  securityContext:
    fsGroup: 0
    fsGroupChangePolicy: OnRootMismatch
  containers:
    - name: agentic-api
      securityContext:
        allowPrivilegeEscalation: false
        capabilities:
          drop: ["ALL"]
        runAsNonRoot: true
        seccompProfile:
          type: RuntimeDefault
```

Mount writable storage at `/var/lib/agentic-api` only when using SQLite. PostgreSQL deployments do not need a persistent filesystem for the gateway.
