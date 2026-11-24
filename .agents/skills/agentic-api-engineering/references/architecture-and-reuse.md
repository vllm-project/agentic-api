# Architecture and reuse discovery

Paths below are relative to the repository root. This is a navigation aid, not a substitute for reading the current
`ARCHITECTURE.md` and source. Names and ownership may change as main evolves.

## Scan the whole repository

Start with an inventory, then follow behavior across crate boundaries:

```bash
git status --short
git log -5 --oneline
rg --files --hidden -g '!.git' -g AGENTS.md -g Cargo.toml -g SKILL.md -g '*.rs' -g '*.py' -g '*.sh'
rg --files --hidden docs .github scripts tests python .agents/skills
rg '^#{1,3} ' ARCHITECTURE.md
```

Read root and any nested agent instructions, terminology, architecture, workspace/package manifests, module roots,
public exports, `.github/workflows/rust.yml`, applicable Python workflows, and `.pre-commit-config.yaml`. Include tests,
benchmarks, fixtures, migrations, and feature-gated code in the map. Use the file inventory and module declarations to
understand all workspace components; deep-read the dependency and execution paths affected by the task. Do not limit
searches to changed files or treat a large generated capture as architectural documentation.

## Ownership map

| Area | Read and trace |
| --- | --- |
| `crates/agentic-server/` | HTTP/WebSocket handlers, route selection, raw proxy versus executor admission, transport errors, OpenAPI, CLI/configuration, authentication and telemetry. Handlers call core APIs, not storage directly. |
| `crates/agentic-server-core/src/types/` | Wire/domain enums, serde, request validation, public input/output history, tool declarations, schemas and conversions. |
| Core `tool/` | `declaration.rs`, `normalize.rs`, handler contracts, discovery, registry and execution ownership. |
| Core `executor/` | Request preparation and rehydration, inference, tool rounds, compaction, persistence, sessions, multi-agent state, budgets and cancellation. |
| Core `events/` | Typed upstream event parsing and normalization; trace lifecycle/assembly ownership into executor ingestion and delivery. |
| Core `storage/` | Stored row types, serialization, occurrence/public IDs, migrations, transactions, SQLite/Postgres and tenant boundaries. |
| `crates/agentic-llm-d/` | Split execution and HTTP contracts that also enter core preparation/validation. |
| `crates/agentic-praxis/` | Embedded integration and programmatic callers; do not assume all requests come through HTTP decoding. |
| `crates/agentic-cli-docs/`, `python/`, `tests/python/` | CLI documentation generation, distribution/launcher, installation and packaging tests. |
| Crate `tests/`, cassette scripts, `scripts/`, docs and CI | Observable contracts, existing recorders and test support, required checks, supported deployment/feature combinations. |

For executor streaming, keep the ownership map in `ARCHITECTURE.md` authoritative: transport framing, semantic event
normalization/ingestion, output assembly, relay delivery, and engine orchestration have separate responsibilities.

## Search before introducing an object or function

Search for the domain term and the actual operation. For example:

```bash
rg -n 'to_upstream_request|in_process_feature|normalized_model_input' crates
rg -n 'ToolDeclaration|to_function_tools|fn validate' crates/agentic-server-core/src
rg -n 'InOutItem|as_inout|as_input|input_items|into_input_items|into_canonical_items' crates
rg -n 'impl.*(From|TryFrom)|Deserialize|deserialize_with|serde\(' crates/agentic-server-core/src/types
rg -n 'carry_over|async_records|compaction|rehydrat' crates/agentic-server-core/src
```

For each candidate, inspect its implementation and all callers, not just the matching signature. Determine what it
borrows/owns, validates, drops, normalizes, or persists; inspect tests for intentional exceptions. Then choose:

- Extend the existing enum/serde contract for a new accepted item shape, preserving malformed-input rejection.
- Extend input-content validation on its owning input type, rather than attaching input history rules to an unrelated
  tool-declaration validator.
- Extend declaration validation/normalization and the existing handler for a supported tool's parameters or execution.
- Reuse an existing storage conversion rather than adding a second metadata-to-JSON/sanitization path.
- Reuse the shared input/output history representation instead of adding another conversation/history wrapper with the
  same purpose. Do not merge types that intentionally have different wire or lifecycle invariants.
- Introduce a small type only when it expresses a missing invariant or ownership/result boundary that existing types
  cannot represent cleanly. Prefer concrete types; introduce generics for actual callers, not hypothetical reuse.

Before removing an implementation, search usages, re-exports, tests, feature gates, and external/public API exposure.
After the edit, repeat the search to catch obsolete paths. If a separate refactor is needed, explain its scope rather
than sneaking it into the feature fix.
