# Getting Started

## Prerequisites

- Rust toolchain (MSRV 1.88)
- [pre-commit](https://pre-commit.com/)

## Building

Install pre-commit hooks and build the project:

```console
pre-commit install
cargo build
```

## Testing

```console
cargo test
```

## Running a harness session

Build both binaries, then let the CLI start Agentic API and configure the harness in an isolated temporary home:

```console
cargo build -p agentic-server --bins
./target/debug/agentic run codex --model Qwen/your-model
./target/debug/agentic run claude --model Qwen/your-model
```

For an existing OpenAI-compatible upstream, pass its `http://` or `https://` base URL with `--upstream`. The harness
model defaults to the first entry from the upstream's `/v1/models`; add `--model` to choose another. SQLite is the
default; pass `--database-url postgresql://...` for a shared PostgreSQL deployment. Use `agentic validate` to check
prerequisites without launching a session.

For a deliberately unattended session in an externally isolated environment, add `--yolo`. This skips Claude Code
permission checks and disables Codex approvals and sandboxing.

Claude sessions always receive the compatible `medium` effort in both the CLI argument and `CLAUDE_CODE_EFFORT_LEVEL`,
because Claude Code gives the environment variable precedence over `--effort`. Qwen3.8-27B's vLLM chat template
accepts `low`, `medium`, and `xhigh`; Claude Code's default `high` produces a template `ValueError`. Override the
pinned value with `AGENTIC_CLAUDE_EFFORT`. See [Harness CLI Testing](../guides/harness-cli-testing.md) for the full
local and Kubernetes verification checklist.

## Linting and Formatting

```console
cargo clippy --all-targets -- -D warnings   # lint
cargo fmt                                     # format
cargo fmt -- --check                          # check formatting only
```

To run all pre-commit hooks manually:

```console
pre-commit run --all-files
```

## Shared Build Cache with sccache

[sccache] caches compiled artifacts so that switching
branches, cleaning `target/`, or working across
multiple git worktrees does not require rebuilding
every dependency from scratch.

### Setup

[Install sccache][sccache-install], then add the
following to your shell profile (`~/.bashrc`,
`~/.zshrc`, etc.):

```sh
export RUSTC_WRAPPER=$(which sccache)
```

### Warming the cache

After setting up sccache, run a full clippy pass in any
worktree to populate the cache:

```console
cargo clippy --workspace --all-targets
```

Subsequent builds reuse the cached artifacts
automatically. Cargo still prints `Compiling` /
`Checking` for every crate, but cache-hit compilations
complete in milliseconds instead of seconds.

Check hit rates with `sccache --show-stats`. See
[sccache usage][sccache-usage] for more configuration
options.

[sccache]: https://github.com/mozilla/sccache
[sccache-install]: https://github.com/mozilla/sccache#installation
[sccache-usage]: https://github.com/mozilla/sccache#usage

## Releases

Maintainers should follow the [release guide](releases.md) to prepare a version and trigger the GitHub Actions
workflows that validate and publish to crates.io and PyPI. Merging a PR does not publish a package.
