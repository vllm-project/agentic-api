---
name: rust-skills
description: Apply idiomatic Rust ownership, typed API design, serde, error handling, async, numeric safety, and lint guidance when implementing, refactoring, or reviewing Rust in agentic-api. Includes a focused set of rules selected for this gateway.
license: MIT
metadata:
  upstream-author: leonardomso
  upstream-version: "1.5.1"
  upstream-repository: https://github.com/leonardomso/rust-skills
  upstream-commit: fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9
---

# Rust design for agentic-api

Apply this skill with the shared [engineering workflow](../agentic-api-engineering/SKILL.md). If that preparation
is already complete, continue from it; do not restart discovery. This skill supplies Rust design guidance, while the
engineering workflow owns architecture discovery, provider parity, recorder usage, and verification.

## Repository constraints come first

Read root `AGENTS.md`, `Cargo.toml`, and `clippy.toml`. The bundled upstream rules are general reference material;
the following agentic-api requirements take precedence over conflicting examples or recommendations:

- Fix Clippy warnings at their cause. Do not add `allow`/`expect` attributes or weaken lint configuration to pass
  checks. Use the workspace lint configuration; do not add per-crate overrides to bypass it.
- Keep `unsafe_code = "forbid"`; do not relax the workspace policy.
- Put `use` imports at the top of the file. Use imported names in implementation code instead of inline paths such
  as `crate::name::some_name::Type` or `crate::name::some_name::function()`. Resolve naming conflicts with import
  aliases (`use ... as ...`), not fully qualified paths in the middle of the code.
- Use the repository's MSRV, edition, dependency versions, and deployment targets. Do not copy generic Cargo,
  runtime, feature, allocator, or compiler settings into the project without a task-specific need and evidence.
- Search existing types, methods, traits, conversions, and callers before introducing another implementation.
  Extend the responsible owner; avoid duplicate validators, normalization paths, string allowlists, and scattered
  per-kind workarounds. A new wrapper, builder, trait, newtype, lifetime, or generic parameter needs a concrete
  invariant or caller benefit. Keep simple concrete code simple.
- Prefer derived serde and existing conversions. Match the actual wire contract: unknown-field rejection,
  defaults, omitted versus null fields, and enum fallback behavior require protocol evidence. Do not apply generic
  `deny_unknown_fields`, `skip_serializing_if`, or `non_exhaustive` advice mechanically.
- Borrow or move before cloning. Use `Cow`, `Arc`, collection wrappers, and preallocation only when the actual
  ownership or measured workload warrants them. Do not replace a readable match with repeated collect chains.
- Preserve checked numeric semantics. A range-check example followed by `as` is not proof that precision,
  truncation, signedness, and the repository's Clippy requirements are satisfied. Inspect boundary cases and use
  existing checked conversion facilities where available; never suppress warnings to retain the cast.

## Read rules for the affected design

These 20 rules cover recurring gateway design concerns. Read only the rows relevant to the changed path.
Architecture discovery, provider parity, and tests remain in the engineering skill rather than a second rule catalog.

| Concern | Rules |
| --- | --- |
| Simple typed design | [Avoid over-abstraction](rules/anti-over-abstraction.md), [avoid stringly typed APIs](rules/type-no-stringly.md), [exhaustive enum matches](rules/pat-exhaustive-enum.md) |
| Ownership and conversions | [Borrow before cloning](rules/own-borrow-over-clone.md), [accept slices](rules/own-slice-over-vec.md), [move out without cloning](rules/mem-take-replace.md), [avoid intermediate collections](rules/anti-collect-intermediate.md), [fallible conversions](rules/conv-tryfrom-fallible.md) |
| Wire types and validation | [Enum tagging](rules/serde-enum-representation.md), [defaults and compatibility](rules/serde-default-compat.md), [derive with validation](rules/serde-try-from-validate.md) |
| Errors and arithmetic | [Typed library errors](rules/err-thiserror-lib.md), [preserve error sources](rules/err-source-chain.md), [checked casts](rules/num-cast-try-from.md), [explicit overflow handling](rules/num-overflow-explicit.md) |
| Async execution and streaming | [Locks across await](rules/async-no-lock-await.md), [bounded channels](rules/async-bounded-channel.md), [cancellation safety](rules/async-cancel-safety.md), [structured task ownership](rules/async-joinset-structured.md) |
| Optimization | [Profile before optimizing](rules/perf-profile-first.md) |

Check design choices against the relevant [Rust Cookbook](https://rust-lang-nursery.github.io/rust-cookbook/)
recipe and [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/). Verify APIs and caveats in official
standard-library, Serde, and dependency documentation for the versions used here. Catalog examples are illustrative,
not a substitute for type-checking, Clippy, contract tests, or understanding agentic-api's existing architecture.

For a review, tie each finding to a concrete changed path, observable failure or maintenance burden, and the smallest
clean fix. Do not turn a generic recipe into a mandatory abstraction or report unsupported stylistic preferences.

## Source and maintenance

The selected rules are adapted from
[leonardomso/rust-skills at fd2a861](https://github.com/leonardomso/rust-skills/tree/fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9),
version 1.5.1. Copyright and MIT terms are retained in [LICENSE](LICENSE). The entrypoint is specific to agentic-api;
rule text retains upstream examples with whitespace cleaned and cross-references to omitted rules removed.

Keep this selection focused on demonstrated needs in agentic-api. Generic naming, packaging, macros, unsafe code,
compiler tuning, and specialized collection guides are intentionally omitted. Before adding a rule, check whether
this skill or the engineering workflow already covers the concern. Preserve attribution and validate links when
updating; do not import the entire upstream catalog or replace repository constraints with upstream defaults.
