# OIDC bearer authentication

## Scope

vLLM Agentic API can optionally authenticate API callers with JSON Web Tokens issued by an OpenID Connect (OIDC)
provider. This is the first authentication slice for [issue #104](https://github.com/vllm-project/agentic-api/issues/104):
it establishes a verified principal at the HTTP boundary without adding a browser login, callback, or server-side
session.

The bearer-token model works with API clients such as Codex and Claude Code. GitHub login can be supplied by an OIDC
provider configured to federate GitHub identities; the gateway does not add GitHub-specific authorization logic.

## Configuration and startup

Authentication is disabled unless both `OIDC_ISSUER` and `OIDC_AUDIENCE` are set. Supplying only one is a startup
error. When enabled, the gateway:

1. fetches the issuer's `/.well-known/openid-configuration` document without following redirects;
2. requires the discovered issuer to match the configured issuer;
3. fetches and caches the JSON Web Key Set (JWKS);
4. refuses to listen if discovery or the initial JWKS request fails.

Issuer and JWKS URLs must use HTTPS. HTTP is accepted only for literal loopback IP addresses (`127.0.0.1` or `::1`)
in local tests and development, and an HTTPS issuer cannot redirect JWKS retrieval to loopback HTTP. Provider
responses are limited to 1 MiB and JWKS documents to 100 keys.

Verification keys are cached for the provider's `Cache-Control: max-age` duration, capped at one hour, or five minutes
when no cache lifetime is supplied. A stale cache is refreshed before a cached key is accepted, so a provider can
remove a compromised key without requiring a gateway restart. While cached keys remain fresh, unknown key IDs can
trigger at most one refresh per 30-second cooldown after a completed fetch. Refreshes are single-flight, and every
successfully fetched key set is installed even when it does not contain the key requested by the triggering token.
Concurrent refresh waiters reuse a result that completes within one second; longer waits return `503 Service
Unavailable` so stalled provider fetches cannot accumulate unbounded request waiters. After a refresh failure, another
provider request is suppressed for 30 seconds and callers receive `503 Service Unavailable`; a one-second coalescing
window also prevents a provider-supplied zero-second cache lifetime from causing one fetch per concurrent request
after the cache expires.

## Request boundary

`/health` and `/ready` remain public so orchestrators can probe the process. Every `/v1/*` route requires
`Authorization: Bearer <token>` when OIDC is enabled, including HTTP streaming and the Responses WebSocket upgrade.
`/ready` continues to report inference-service readiness; it does not treat a temporary identity-provider refresh
failure as a reason to remove an otherwise healthy gateway from service. Those request-time dependency failures
return `503 Service Unavailable` as described above.

The gateway verifies:

- an asymmetric token signing algorithm and a signature from the provider JWKS;
- a signing key whose `kid`, `alg`, `use`, and `key_ops` permit verification;
- required `iss`, `aud`, `sub`, and `exp` claims;
- issuer equality; every `aud` value equal to the configured audience; and any present `azp` value equal to it;
- expiration and, when present, the not-before time.

Successful authentication inserts the stable issuer and subject pair into request extensions as the authenticated
principal. Tenant and persisted-state authorization remain follow-up work under
[issue #107](https://github.com/vllm-project/agentic-api/issues/107).

For WebSockets, authentication occurs during the HTTP upgrade. The validated expiration is retained with the
principal, and the gateway rejects new `response.create` messages after the token expires (including clock skew).

## Credential separation

The verified identity token is consumed at the gateway and is never forwarded to the inference service. OpenAI-style
upstream requests use `OPENAI_API_KEY` after authentication removes the inbound `Authorization` header.
Anthropic-compatible requests may continue to supply an upstream `x-api-key`; otherwise they also fall back to
`OPENAI_API_KEY`.

This separation prevents an OIDC identity token from being mistaken for an inference-provider credential. Deployments
that do not enable OIDC retain the existing pass-through behavior for client credentials on executor routes. Routes
that read or write local persisted state instead require the configured `OPENAI_API_KEY` as a bearer token: stored
response retrieval and deletion, response `input_items`, conversations and their items, and file/vector-store routes.
These routes cannot delegate credential validation to the inference service.
