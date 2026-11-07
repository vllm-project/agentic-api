# Security Policy

This policy covers vLLM Agentic API in this repository. Agentic API provides
stateful APIs, persistence, and gateway-executed tools in front of inference
services such as vLLM. Issues confined to the vLLM inference engine should be
reported under the [vLLM security policy](https://github.com/vllm-project/vllm/security/policy).
Please report issues involving Agentic API's integration with an inference
service here.

## Supported versions

We welcome reports affecting the current `main` branch or any published release.
Please identify the version or commit you tested. The maintainers will determine
which releases are affected and whether a fix can be backported; this repository
does not currently publish a fixed backport schedule.

## Reporting a vulnerability

Use **Report a vulnerability** on this repository's
[Security page](https://github.com/vllm-project/agentic-api/security) when that
option is available. This sends the report privately to repository maintainers.

If that option is unavailable, open a public issue **only to ask for a private
reporting contact**. Do not include the vulnerability, reproduction steps,
exploit code, credentials, or affected users' data in that issue. Do not disclose
vulnerability details in a public pull request or discussion.

Please include, as applicable:

- A concise description of the issue and its potential impact.
- The affected version or commit and relevant deployment configuration.
- The access or privileges an attacker needs.
- Minimal steps to reproduce in a system you control, or a clear source-code
  explanation when the issue has not been reproduced.
- Relevant code locations and sanitized logs or output.
- Any mitigation you have tested, and what remains uncertain.

Do not send real API keys, database credentials, identity tokens, personal data,
or other secrets. If automated tools assisted your investigation, distinguish
their suggestions from behavior you verified.

## Security scope

Relevant security boundaries include the HTTP and WebSocket gateway, caller
authentication and authorization, stored responses and conversations,
gateway-executed tools and their network access, the llm-d state backend,
credential handling, the CLI and Python launcher, and the project's published
packages and container images.

Please describe any conditions needed to reach the issue, including external
authentication, network exposure, shared users, optional features, or local
services. A deployment-specific condition does not by itself make a report
irrelevant. The maintainers will assess severity using the reachable impact and
the conditions required to exploit it.

## Coordinated disclosure

Please allow the maintainers to investigate and coordinate a fix before public
disclosure. The maintainers will assess affected versions and severity during
triage and coordinate any advisory or reporter credit through the private
reporting channel.
