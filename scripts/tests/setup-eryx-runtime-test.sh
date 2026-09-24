#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repository_root="$(cd -- "${script_dir}/../.." && pwd -P)"
test_root="$(mktemp -d)"
trap 'rm -r "$test_root"' EXIT

fake_precompiler="${test_root}/eryx-precompile"
cat >"$fake_precompiler" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  --version)
    echo "eryx-precompile ${FAKE_ERYX_VERSION:?}"
    ;;
  setup)
    : >"${FAKE_ERYX_SETUP_MARKER:?}"
    ;;
  *)
    echo "unexpected argument: ${1:-}" >&2
    exit 2
    ;;
esac
EOF
chmod +x "$fake_precompiler"

eryx_version="$(awk '
  $0 == "name = \"eryx\"" { found = 1; next }
  found && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "${repository_root}/Cargo.lock")"
marker="${test_root}/setup-called"

"${repository_root}/scripts/setup-eryx-runtime.sh" --help | grep -q '^Usage:'

if "${repository_root}/scripts/setup-eryx-runtime.sh" unexpected >"${test_root}/unexpected.out" 2>&1; then
  echo "setup script accepted an unexpected argument" >&2
  exit 1
fi
grep -q 'error: unexpected argument' "${test_root}/unexpected.out"

if env \
  ERYX_PRECOMPILE_BIN="$fake_precompiler" \
  FAKE_ERYX_VERSION=0.0.0 \
  FAKE_ERYX_SETUP_MARKER="$marker" \
  "${repository_root}/scripts/setup-eryx-runtime.sh" >"${test_root}/mismatch.out" 2>&1; then
  echo "setup script accepted a mismatched eryx-precompile version" >&2
  exit 1
fi
grep -q "Eryx ${eryx_version} is required" "${test_root}/mismatch.out"
test ! -e "$marker"

env \
  ERYX_PRECOMPILE_BIN="$fake_precompiler" \
  FAKE_ERYX_VERSION="$eryx_version" \
  FAKE_ERYX_SETUP_MARKER="$marker" \
  "${repository_root}/scripts/setup-eryx-runtime.sh" >"${test_root}/success.out"
test -f "$marker"
grep -q 'cargo build --release -p agentic-server --features embedded-code-interpreter' "${test_root}/success.out"

shadow_dir="${test_root}/shadow-bin"
install_root="${test_root}/cargo-home"
installer_log="${test_root}/installer.log"
install_marker="${test_root}/installed-setup-called"
mkdir -p "$shadow_dir"

cat >"${shadow_dir}/eryx-precompile" <<'EOF'
#!/usr/bin/env bash
echo 'eryx-precompile 0.0.0'
EOF
chmod +x "${shadow_dir}/eryx-precompile"

installed_precompiler="${test_root}/installed-eryx-precompile"
cat >"$installed_precompiler" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  --version)
    echo "eryx-precompile ${FAKE_INSTALLED_ERYX_VERSION:?}"
    ;;
  setup)
    : >"${FAKE_INSTALLED_SETUP_MARKER:?}"
    ;;
  *)
    exit 2
    ;;
esac
EOF
chmod +x "$installed_precompiler"

cat >"${shadow_dir}/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "binstall" && "${2:-}" == "--version" ]]; then
  [[ "${FAKE_BINSTALL_AVAILABLE:-0}" == "1" ]]
  exit
fi

subcommand="${1:-}"
shift
printf '%s %s\n' "$subcommand" "$*" >>"${FAKE_INSTALLER_LOG:?}"
install_root=""
while (( $# != 0 )); do
  if [[ "$1" == "--root" ]]; then
    install_root="$2"
    break
  fi
  shift
done
if [[ -z "$install_root" ]]; then
  echo 'missing --root' >&2
  exit 2
fi
mkdir -p "${install_root}/bin"
cp "${FAKE_INSTALLED_PRECOMPILER:?}" "${install_root}/bin/eryx-precompile"
chmod +x "${install_root}/bin/eryx-precompile"
EOF
chmod +x "${shadow_dir}/cargo"

for installer in install binstall; do
  rm -r "$install_root" 2>/dev/null || true
  rm -f "$install_marker" "$installer_log"
  binstall_available=0
  if [[ "$installer" == "binstall" ]]; then
    binstall_available=1
  fi

  env \
    PATH="${shadow_dir}:/usr/bin:/bin" \
    CARGO_HOME="$install_root" \
    ERYX_PRECOMPILE_INSTALL_ROOT="$install_root" \
    FAKE_BINSTALL_AVAILABLE="$binstall_available" \
    FAKE_INSTALLER_LOG="$installer_log" \
    FAKE_INSTALLED_PRECOMPILER="$installed_precompiler" \
    FAKE_INSTALLED_ERYX_VERSION="$eryx_version" \
    FAKE_INSTALLED_SETUP_MARKER="$install_marker" \
    "${repository_root}/scripts/setup-eryx-runtime.sh" >"${test_root}/${installer}.out"

  test -f "$install_marker"
  grep -q "^${installer} .*--root ${install_root} .*--version =${eryx_version} eryx-precompile" "$installer_log"
done
