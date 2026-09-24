#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Prepare the Eryx runtime required by the embedded code-interpreter Cargo feature.

Usage:
  ./scripts/setup-eryx-runtime.sh

The script installs the eryx-precompile version matching Cargo.lock when needed,
then downloads and precompiles the platform-specific runtime into Eryx's user
cache. Set ERYX_PRECOMPILE_BIN to use a specific eryx-precompile executable.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi
if (( $# != 0 )); then
  echo "error: unexpected argument: $1" >&2
  usage >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repository_root="$(cd -- "${script_dir}/.." && pwd -P)"

eryx_version="$(awk '
  $0 == "name = \"eryx\"" { found = 1; next }
  found && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "${repository_root}/Cargo.lock")"
if [[ -z "$eryx_version" ]]; then
  echo "error: unable to determine the locked Eryx version" >&2
  exit 1
fi

precompile_bin="${ERYX_PRECOMPILE_BIN:-}"
if [[ -n "$precompile_bin" && ! -x "$precompile_bin" ]]; then
  echo "error: ERYX_PRECOMPILE_BIN is not executable: $precompile_bin" >&2
  exit 2
fi
if [[ -z "$precompile_bin" ]]; then
  precompile_bin="$(command -v eryx-precompile || true)"
fi

installed_version=""
if [[ -n "$precompile_bin" ]]; then
  installed_version="$($precompile_bin --version 2>/dev/null | awk '{print $NF}')"
fi

if [[ "$installed_version" != "$eryx_version" ]]; then
  if [[ -n "${ERYX_PRECOMPILE_BIN:-}" ]]; then
    echo "error: $precompile_bin is version ${installed_version:-unknown}; Eryx $eryx_version is required" >&2
    exit 2
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required to install eryx-precompile $eryx_version" >&2
    exit 127
  fi
  install_root="${ERYX_PRECOMPILE_INSTALL_ROOT:-${CARGO_HOME:-}}"
  if [[ -z "$install_root" ]]; then
    if [[ -z "${HOME:-}" ]]; then
      echo "error: HOME, CARGO_HOME, or ERYX_PRECOMPILE_INSTALL_ROOT is required to install eryx-precompile" >&2
      exit 2
    fi
    install_root="${HOME}/.cargo"
  fi

  echo "Installing eryx-precompile $eryx_version"
  if cargo binstall --version >/dev/null 2>&1; then
    cargo binstall --no-confirm --locked --root "$install_root" --version "=$eryx_version" eryx-precompile
  else
    cargo install --locked --root "$install_root" --version "=$eryx_version" eryx-precompile
  fi
  precompile_bin="${install_root}/bin/eryx-precompile"
  if [[ ! -x "$precompile_bin" ]]; then
    echo "error: eryx-precompile was installed but is not executable: $precompile_bin" >&2
    exit 1
  fi
fi

installed_version="$($precompile_bin --version 2>/dev/null | awk '{print $NF}')"
if [[ "$installed_version" != "$eryx_version" ]]; then
  echo "error: $precompile_bin is version ${installed_version:-unknown}; Eryx $eryx_version is required" >&2
  exit 2
fi

echo "Preparing the Eryx $eryx_version runtime for this platform"
"$precompile_bin" setup
echo "Eryx runtime ready; build with:"
echo "  cargo build --release -p agentic-server --features embedded-code-interpreter"
