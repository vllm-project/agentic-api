# Provider-aware reasoning replay: implementation slices

Tracking issue: [#335](https://github.com/vllm-project/agentic-api/issues/335).

## Current slice: typed reasoning foundation

This slice does **not** enable opaque reasoning replay. The executor still uses the
existing vLLM replay policy: join usable plaintext content, omit summaries from the
upstream copy, and reject opaque-only continuation before normal inference.
Canonical public and persisted items retain their complete reasoning representation.

`types/io/reasoning.rs` owns the new pure wire types:

- `ReasoningTextContent` has a closed `reasoning_text` discriminator.
- `ReasoningSummaryContent` has a closed `summary_text` discriminator and string text.
- `ReasoningStatus` accepts `in_progress`, `completed`, and `incomplete`.
- `OpaqueReasoning` preserves `encrypted_content` as an exact string. It has no
  decoding or normalization operation, redacts `Debug`, and rejects values larger
  than 16 MiB of decoded UTF-8. This is a gateway ceiling, not a provider limit.

These types replace arbitrary JSON in `ReasoningOutput`. Existing Rust callers
constructing fields directly must use the new types. Missing or null `content` and
`summary` still deserialize as empty arrays; missing or null encrypted state and
status remain `None`. Ingestion retains the existing completion distinction:
omitted arrays preserve accumulated parts, while explicit null or empty arrays clear
them. Public JSON field names and valid string-valued opaque state do not change.

The existing request/body ceilings and shared retained-response budget still apply.
Reasoning parts, including empty parts, carry a structural charge; all retained text
and opaque bytes are charged through `RetainedSize`. No new queue, task, parser,
delivery path, or inference policy is introduced. The 16 MiB per-value ceiling is not
a claim about aggregate process memory; the usual retained-response budget is smaller.

Reasoning rows written by earlier releases stay readable. When a stored reasoning
row fails typed decoding, a bounded compatibility projection (at most 16 MiB of JSON
and 4096 content parts) keeps its plaintext in order as `reasoning_text` and keeps
every summary part, string opaque state, and status value that still decodes. It
drops only untyped values, so such history remains continuable on the vLLM path.

Rows that cannot be read even this way now fail closed: both stores return
`StorageError::InvalidHistoryItem` instead of skipping them, and response history
also rejects a missing referenced row. Reasoning therefore never silently disappears
from a continuation. Database rows are neither rewritten nor deleted, and no SQL
migration is required for this slice. Valid existing records keep their wire representation.
Response history references and effective metadata now also decode fallibly. Malformed
JSON, wrong field types, and explicit JSON `null` fail closed instead of becoming empty
history or default settings. SQL NULL retains its existing legacy behavior; it does not
establish replay provenance. A missing captured conversation response or a reference
to another conversation is an error when loading versioned metadata. Parse diagnostics
are intentionally excluded from storage errors because they may echo stored secrets.
These checks do not establish provider identity or authorize opaque replay.

## Remaining slices before enabling a provider profile

1. Add explicit server-owned replay-domain configuration, binding provider endpoint,
   credential realm, compatible model family, and opaque format. Never select a
   permissive policy from the client-supplied model name alone.
2. Carry typed, versioned per-item provenance through durable history and transient
   session checkpoints, extending the fail-closed metadata decoding with a storage migration.
   Distinguish provider-issued state from manually submitted state; successful
   upstream acceptance must not silently upgrade manual provenance.
3. Project compatible reasoning only in the upstream request copy. Keep the single
   `OutputItem::to_input_item` conversion and existing ingestion path. Select strict
   terminal validation for the opaque profile rather than introducing a second
   state machine. Retain the vLLM default.
4. Separate local plaintext compaction checkpoints from provider opaque compaction.
   Reject unsupported combinations before inference. Qualify tool-round ordering,
   branching, HTTP/SSE, and transient WebSocket continuations with recorded exchanges.
5. Close the streaming producer abort/join gap with #244 before production enablement.

The upstream contract requires preserving opaque state and limits reasoning reuse
to compatible model families; see the
[official reasoning guide](https://developers.openai.com/api/docs/guides/reasoning).
The gateway must not infer compatibility from the presence of `encrypted_content`.

## Verification

`reasoning_test.rs` replays the existing recorder-generated OpenAI and gateway JSON
and SSE cassettes and checks content, summaries, opaque state, status, identity, and
the output-to-input conversion. `reasoning_types_test.rs` covers schema rejection,
nullability, exact string round trips, redaction, and the decoded-byte ceiling.
Accumulator tests cover malformed strict completion and retained-budget exhaustion,
including arrays of empty typed parts. Storage tests cover invalid and missing rows;
stateful and session tests retain the existing vLLM continuation behavior.
`storage_response_integrity_test.rs` additionally verifies invalid metadata and history
references, legacy SQL NULL handling, missing/foreign captured turns, error redaction,
and refusal to persist a child of an invalid parent. A recorded initial exchange checks
that malformed continuation metadata fails before either JSON or SSE inference starts.
`legacy_reasoning_rows_test.rs` rewrites stored rows to shapes earlier releases accepted
and checks that each decodes with its valid fields kept, and that a `previous_response_id`
continuation replays its plaintext to vLLM as `reasoning_text`.

No captured YAML was hand-authored or modified for this slice. Future provider replay
scenarios must use the cassette README's recorder workflow and staged validation.

The workspace suite (including OpenAPI and cassette tests), Clippy with warnings
denied, and formatting checks passed with Rust 1.98. Opt-in ignored tests were not run:

```bash
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
```

Rust 1.85 verification currently stops at dependency MSRV checks: the existing
lockfile includes dependencies requiring Rust 1.86–1.88. This slice does not change
the dependency graph. Dependency and baseline language compatibility need a separate
MSRV repair before the repository can claim the documented 1.85 release gate.
