# PR description

Keep it short enough to read in two minutes. Reviewers read the code; the description tells them where to look and
why the change is shaped the way it is.

## Required sections

```markdown
## Summary

One or two sentences: the problem a real client hits (or the provider mismatch a recording shows) and the fix.
Link the issue.

## Changes by API

| API | What changed |
| --- | --- |
| Responses (`/v1/responses`) | ... |
| Conversations (`/v1/conversations`) | ... |
| Messages (`/v1/messages`) | ... |
| Files and vector stores | ... |

List only the APIs this PR changes; write "no wire change" for internal changes.

## Test Plan

The recordings added or updated (scenario and file names), the replay test, and the commands run with their results.
```

## Features, refactors, and layer or module changes

Add a **Flow** section with a small Mermaid diagram of the path the PR changes, marking what is new or different.
Keep it to the boxes a reviewer needs. Small fixes, tests, and documentation changes do not need one.

When a module or layer is added, also add a short **New modules** section: one line per new module saying what it
owns and which existing owner it sits next to. Attach a class diagram as a supplementary document (a collapsed
`<details>` block or a linked Markdown file) showing the new types and the existing types they connect to. Do not add
a class diagram for changes that only modify existing types.

## Example

```markdown
## Summary

A Responses request naming an unknown `conversation` created it instead of returning OpenAI's 404 (#155).

## Changes by API

| API | What changed |
| --- | --- |
| Responses (`/v1/responses`) | an unknown `conversation` returns `404 invalid_request_error`, `code` and `param` null |
| Conversations (`/v1/conversations`) | no wire change; implicit creation removed |

## Test Plan

- `response-conversation` scenario: four turns naming a deleted or unknown conversation, recorded against OpenAI and
  the gateway; `responses_api_test` replays them.
- `cargo test -p agentic-server-core --test responses_api_test`: 31 passed.
```

(Remove the space inside the inner fences when copying.)
