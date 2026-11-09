---
name: agentic-api-engineering
description: Prepare, implement, and review agentic-api code changes using repository architecture, existing typed Rust extension points, provider documentation, and regression evidence. Use before feature work, fixes, refactoring, or code review in this repository.
---

# Agentic API Engineering

Make the smallest clean change that addresses the actual problem and fits the existing architecture. For reviews,
apply the same criteria to the complete diff and its callers. Keep reviews read-only unless the user asks for fixes.
A focused PR is not authorization for a broader refactor, feature expansion, commit, push, or external review comment.

## Before coding or reviewing

1. Establish the branch, base/head or working-tree diff, and user-authorized scope. Preserve unrelated local work.
2. Read `AGENTS.md`, `TERMINOLOGY.md`, and `ARCHITECTURE.md`. **Scan the whole repository**, not just the reported file:
   inventory workspace crates, module roots, public APIs, tests and cassettes, Python tooling, scripts, configuration,
   docs, CI, and existing skills. Use [architecture and reuse](references/architecture-and-reuse.md) to build the map.
   Read the affected implementation, callers, conversions, and tests deeply. Scanning does not mean loading every
   large YAML capture or generated file. Reuse the map within a session; refresh it after relevant branch/main changes.
3. Reproduce or trace the actual failure. Separate the issue author's explanation from observed behavior. Identify
   which component rejects, drops, transforms, executes, or stores the request. Do not infer gateway behavior from an
   upstream error or infer execution support just because a wire type can deserialize.
4. For Rust changes, read the bundled [Rust skill](../rust-skills/SKILL.md) and the rules relevant to the affected
   design. Apply its agentic-api constraints before the generic examples; read individual rules on demand rather
   than loading the entire catalog. The skill is repository-local and needs no personal installation.
5. Consult the relevant [Rust Cookbook](https://rust-lang-nursery.github.io/rust-cookbook/) recipes and
   [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) for the design. Check current standard-library,
   Serde, and crate documentation against the repository's MSRV and locked dependencies; do not blindly copy old
   recipes. For API behavior, consult current official OpenAI or Anthropic documentation for the affected surface;
   consult both when adapting between them. Follow [provider parity and recorders](references/parity-and-recorders.md).
   Do not conclude that an API behavior or edge case is correct, a regression, or provider parity without observed
   evidence from the relevant provider API: OpenAI Responses for Responses behavior, Anthropic Messages for Messages
   behavior, and both for cross-provider claims. A gateway-only reproduction is a finding to investigate, not proof
   of the provider contract; label conclusions without provider evidence as unverified.

Before the first edit, give a short account of the affected path, existing extension point, intended behavior, and
verification plan. This is an explanation, not an extra approval gate. Resolve routine design choices autonomously.
For a review, establish these facts internally and report findings with evidence.

## Find the existing owner before adding code

Before adding any type, enum variant, method, helper, module, validator, serializer, or conversion:

- Search for the behavior as well as the proposed name. Inspect existing implementations and their callers, including
  `From`/`TryFrom`, serde attributes, validation methods, normalization, replay, and storage conversions.
- Prefer extending the existing responsible type, enum, or method when that keeps its contract coherent. Explain why
  reuse is insufficient before introducing a new abstraction. A useful new type encodes an invariant, ownership
  boundary, or distinct domain concept; a wrapper around an iterator chain does not justify itself.
- Before adding an enum, ID type, record, or validation rule, search `types/` and sibling modules for one that already
  models the concept, and use it. An enum mapped variant by variant onto another enum, a `String` where a validated
  newtype exists or a second copy of an acceptance rule is duplication; extend the shared
  type instead. Shared contracts live in `types/`, so an executor-only copy cannot replace them.
- Do not scatter free functions, string allowlists, per-kind conditionals, or parallel parsers across files to make
  one case pass. Keep one authoritative implementation of the rule. A small free function is acceptable when it has
  a clear module responsibility and no natural existing owner; the objection is ad hoc duplication, not Rust syntax.
- Do not move unrelated logic into an existing object just to avoid a helper. Keep wire validation, tool declaration
  validation, execution, and persistence responsibilities distinct.
- Remove superseded implementations and update all callers when replacing behavior. Search again for obsolete
  helpers, duplicate types, redundant serialization, and dangling functions. Check feature-gated and test callers.
- Keep the scope small. Record broader cleanup separately unless it is necessary for correctness or explicitly asked
  for. Avoid speculative generic frameworks, new history wrappers, or unnecessary lifetime parameters.

See the concrete extension points and search prompts in [architecture and reuse](references/architecture-and-reuse.md).

## Typed Rust and ownership

- Model supported wire shapes with structs/enums and exhaustive matches. Do not use loose `serde_json::Value` dispatch
  at a public boundary to avoid modeling a known protocol. Preserve intentional opaque/raw pass-through contracts;
  strict typed execution and byte-preserving forwarding can have different obligations.
- Prefer derived `Serialize`/`Deserialize`, serde attributes, and existing `From`/`TryFrom` conversions. Use custom
  deserialization only for a demonstrated wire requirement that derive cannot express. Avoid a second outer decoder
  when the item enum can express the contract. Prevent malformed known variants from falling through as shorthand.
- Put invariants on the owning type through a fallible constructor, `TryFrom`, or its existing validation method.
  Verify that every relevant entry point invokes validation, including programmatic construction and rehydration;
  serde alone does not protect values built directly in Rust. Keep public OpenAPI schemas aligned with decoding.
- Preserve the distinction between public history and model-facing normalization. Check whether an existing
  conversion lowers custom/shell calls, drops gateway projections, or applies media defaults before reusing it.
  Similar return types do not prove equivalent semantics.
- Borrow for inspection and move when transferring ownership. Clone only where an independent owner needs the data.
  A slice's `to_vec()` clones its elements; it is not a free ownership conversion. Check nested image/file/tool-result
  payloads, repeated metadata copies, and work done before filtering discarded items.
- Keep transformations readable. Avoid repeated `filter_map`/`collect`/`into_owned` chains at callers, but do not hide
  them behind a new wrapper without a real domain purpose. Use `Cow` only when borrowed-or-owned behavior is needed;
  prefer a simple owned result when selection is complete and the source history need not be retained.
- Avoid caller-maintained parallel arrays for IDs, flags, and serialized records. `zip` can silently truncate unequal
  inputs. Keep related values together in an existing typed record or a justified batch type. An internal mask
  derived from the same immutable collection is different; verify its length and ordering cannot diverge.
- Put `use` imports at the top of the file. Do not write paths such as `crate::name::some_name::Type` or
  `crate::name::some_name::function()` in the middle of implementation code; use the imported names instead.
  Resolve naming conflicts with an import alias (`use ... as ...`), not an inline fully qualified path.
- Preserve typed errors, error sources, and public error envelopes. Use checked conversions and explicit range
  validation; do not silently truncate, change signedness, clamp, or lose precision to satisfy a lint.
- **Do not add `#[allow(clippy::...)]`, `#[expect(clippy::...)]`, crate-wide suppression, or weaker lint configuration
  to bypass warnings.** Fix the underlying design or operation. Do not split logic into arbitrary helpers merely to
  evade a size/complexity lint. Report unrelated pre-existing warnings separately rather than masking them or expanding
  the task into an unrequested cleanup.

## Feature support and regression boundaries

Trace the applicable path from transport through typed input, admission, tools, upstream conversion, events, and
persistence back to the client. Check the routes that bypass a stage as well as the route that executes it.

- Separate parsing support, validation, gateway execution support, upstream support, public output emission, and
  replay support. Adding one does not establish the others. Unsupported typed input must not silently disappear.
- Locate actual route selection and `RequestPayload::to_upstream_request`; tool declarations use the existing
  `ToolDeclaration::validate` and `to_function_tools` path. Do not create a second normalization mechanism.
- Verify `store: false`, durable storage, explicit conversations, and transient WebSocket continuation separately.
  Do not infer proxy routing or normalization from `store` alone, and do not change checkpoint semantics as a side
  effect of a typing fix. Inspect the current implementation rather than preserving a historical assumption.
- Preserve IDs and call/output relationships, public versus internal item classification, replay order, structured
  content, reasoning, metadata, usage, tool limits, and compaction carry-over where the change touches them.
- Check first and later rounds, JSON/SSE/WebSocket paths, raw forwarding versus typed execution, and error behavior as
  applicable. For storage changes, include transactions/rollback, tenant isolation, continuation, and SQLite/Postgres.
- For streaming or executor placement, use the existing
  [executor architecture review](../executor-architecture-review/SKILL.md). Keep its checks read-only during planning
  and review; implementation remains governed by the user's authorized task.
- For schema, migration, stored JSON, or persisted-field changes, use the
  [storage schema change](../storage-schema-change/SKILL.md) checklist.

## Keep documentation synchronized

Update documentation as part of the same change whenever implementation alters what it describes:

- Update `ARCHITECTURE.md` for changes to module responsibilities, request flow, ownership, or extension points.
- Update the root and affected crate/directory README files for changes to supported features, usage, configuration,
  setup, or test and recorder workflows. Verify examples against the final implementation.
- Update the relevant documents in `docs/design/` and other maintained design specifications for changed contracts,
  invariants, decisions, or limitations. Distinguish implemented behavior from proposals; preserve historical
  decision records and link a superseding design when appropriate.

Search for references to renamed or removed types, methods, options, and behaviors so stale explanations do not
remain elsewhere. Extend the existing documentation rather than adding a duplicate design document. During reviews,
check documentation against the final code and identify missing updates. If no documented behavior or design changed,
state that briefly in the completion report; do not make unrelated documentation edits.

## Verification and reporting

Use [verification](references/verification.md) for commands, coverage selection, and truthful results. Use
[provider parity and recorders](references/parity-and-recorders.md) for cassette decisions and concrete Python/shell
examples. Do not substitute gateway-only mocks for evidence of undocumented provider behavior, and do not demand new
recordings for a pure refactor whose contract is already covered.

For reviews, follow [PR review](../pr-review/SKILL.md): inspect the whole diff, order actionable findings by severity,
identify precise paths/lines and triggering scenarios, and give a concrete fix direction using the existing owner.
Distinguish correctness regressions from optional improvements and pre-existing limitations. If no actionable findings
remain, say so. Do not edit files while reviewing unless asked to apply fixes.

For implementation, self-review the final diff against these criteria. Report what changed and why, exact checks and
results, failures and reruns, ignored/skipped coverage and reasons, and remaining provider-parity uncertainty. Do not
call a filtered or interrupted run a full-suite pass. Leave committing and publishing to the user's instructions.
