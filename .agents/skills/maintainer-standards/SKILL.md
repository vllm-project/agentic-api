---
name: maintainer-standards
description: The agentic-api maintainers' decisions on what work is worth a pull request, which core contracts are protected, what a pull request must contain, and how maintainers post reviews. Use before choosing work, opening or reviewing a pull request, or changing the request path (rehydration, engine execution, persistence), ExecutionContext or RequestContext, or types/.
---

# Maintainer Standards

Agentic API is an open-source implementation of the OpenAI Responses and Conversations APIs and the Anthropic
Messages API in front of open-model servers such as vLLM, with the Interactions API planned. Real clients (Codex,
Claude Code, the OpenAI and Anthropic SDKs, curl) must behave as they do against the hosted providers. Work is ranked:
**correctness against the provider contract, maintainability of the core, ease of contribution, measured
performance**.

This skill holds only maintainer decisions. Each rule lives in one place; follow the link instead of restating it.

| Topic | Source of truth |
| --- | --- |
| How the code works: request flow, the input and tool normalizations, what a turn persists | `ARCHITECTURE.md` |
| Finding the existing owner, typed Rust, documentation updates | [engineering](../agentic-api-engineering/SKILL.md) |
| Provider evidence, recorders, when a recording is needed | [parity and recorders](../agentic-api-engineering/references/parity-and-recorders.md) |
| Schema, migrations, client-facing storage | [storage schema change](../storage-schema-change/SKILL.md) |
| Streaming stage ownership | [executor architecture review](../executor-architecture-review/SKILL.md) |
| Findings format and cassette decision in a review | [PR review](../pr-review/SKILL.md) |
| Layer boundaries, the single path, lean contexts, extension points | [protected core](references/protected-core.md) |
| PR description format | [PR description](references/pr-description.md) |

## Worth a pull request

- Fix what a real client hits or what a recording shows differs from the provider. Do not open PRs for improbable
  inputs found by scanning code or fuzzing a validator (unusual host encodings in a URL that no client sends, for
  example). Those PRs are closed, and accounts that open them in bulk are treated as automated.
- A performance claim needs benchmark results against `main` from a clean run (old criterion output and benchmark
  databases removed).
- A refactor must end with fewer lines and simpler interfaces. Moving or reshaping code is not a refactor.

## Protected core

Layer and module boundaries come first: every change names the layer and owner it belongs to and does not reach past
it. On top of that, the request path from rehydration through engine execution to persistence is one path for every
transport and state source; `ExecutionContext` and `RequestContext` stay lean, so an addition is checked for its layer,
its weight, and whether it forces new arguments through the call chain; and `types/` owns the shapes and conversions.
Details and extension points are in [protected core](references/protected-core.md). Changing one of these contracts
needs an RFC issue and maintainer agreement before code is written; the PR names the extension point it used or
links that RFC.

## Pull request requirements

- At most three open PRs per contributor. A stack a maintainer asked for counts as one.
- One concern per PR: a fix carries no unrelated refactor, and a refactor changes no behavior.
- A design issue raised in review is fixed in the same PR, not deferred to a follow-up.
- The description follows [PR description](references/pr-description.md).
- Pre-commit passes, commits are signed off, and subjects are short conventional one-liners.

## How maintainers post reviews

Triage before reading for findings, and stop at the first failure: is the problem real, is the expected behavior
recorded, does the change keep to the protected core. Then find issues with the [PR review](../pr-review/SKILL.md)
skill.

Post one submission: a short body saying where the code lives today and what to extend, plus inline comments that do
not repeat the body. Approve a good PR with "Thanks for the fix!" and optional `nit:` comments. State provider
behavior exactly as the recording shows it. Do not narrate your own verification, offer to "look again", or propose
follow-ups. When a contributor cannot record, a maintainer records the provider reference and the contributor matches
the gateway to it.
