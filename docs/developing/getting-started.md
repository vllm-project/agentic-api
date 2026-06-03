# Getting Started

## Prerequisites

- Rust toolchain (MSRV 1.85)
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
