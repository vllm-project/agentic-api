from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
VALIDATOR = REPO_ROOT / "scripts" / "validate-python-release-version.sh"
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-python.yml"
CRATE_RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-crates.yml"
PYTHON_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "python.yml"
BUILD_CONSTRAINTS = REPO_ROOT / "python-build-constraints.txt"
WORKSPACE_VERSION = re.search(
    r"(?ms)^\[workspace\.package\].*?^version\s*=\s*\"([^\"]+)\"", (REPO_ROOT / "Cargo.toml").read_text()
).group(1)


def test_release_version_validator_accepts_workspace_version() -> None:
    env = os.environ.copy()
    env["AGENTIC_API_RELEASE_VERSION"] = WORKSPACE_VERSION

    result = subprocess.run(["/bin/bash", str(VALIDATOR)], env=env, capture_output=True, text=True, check=False)

    assert result.returncode == 0, result.stderr


def test_release_version_validator_rejects_shell_payload_without_executing_it(tmp_path: Path) -> None:
    marker = tmp_path / "injected"
    env = os.environ.copy()
    env["AGENTIC_API_RELEASE_VERSION"] = f"{WORKSPACE_VERSION}; touch {marker}"

    result = subprocess.run(["/bin/bash", str(VALIDATOR)], env=env, capture_output=True, text=True, check=False)

    assert result.returncode != 0
    assert not marker.exists()
    assert f"{WORKSPACE_VERSION} release workflow" in result.stderr


def test_release_workflow_uses_declared_version_without_dispatch_override() -> None:

    workflow = RELEASE_WORKFLOW.read_text()
    assert "inputs.version" not in workflow
    assert "      version:" not in workflow.split("jobs:", 1)[0]
    assert "AGENTIC_API_RELEASE_VERSION: ${{ needs.release-version.outputs.version }}" in workflow
    version_job = workflow.split("  release-version:", 1)[1].split("  build-wheels:", 1)[0]
    assert 'python-version: "3.12"' in version_job


@pytest.mark.skipif(sys.version_info < (3, 11), reason="Release job uses Python 3.12 with stdlib tomllib")
def test_release_version_reader_follows_cargo_manifest(tmp_path: Path) -> None:
    import shlex
    import textwrap

    workflow = RELEASE_WORKFLOW.read_text()
    block = next(block for block in _workflow_run_blocks(workflow) if "tomllib" in block)
    script = textwrap.dedent(block.removeprefix("|").lstrip("\n"))
    script = script.replace("python3 -", f"{shlex.quote(sys.executable)} -", 1)
    output = tmp_path / "output"
    env = os.environ.copy()
    env["GITHUB_OUTPUT"] = str(output)
    for version in ("0.7.0", "1.2.3"):
        (tmp_path / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
        output.write_text("")
        result = subprocess.run(["bash", "-eu", "-c", script], cwd=tmp_path, env=env, capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        assert output.read_text() == f"version={version}\n"


def test_crate_release_dry_run_packages_server_against_the_local_core() -> None:
    workflow = CRATE_RELEASE_WORKFLOW.read_text(encoding="utf-8")
    dry_run_block = next(block for block in _workflow_run_blocks(workflow) if "Would release commit" in block)
    normalized_block = " ".join(dry_run_block.split())

    for command in (
        "cargo check --workspace --locked",
        "cargo clippy --all-targets --locked -- -D warnings",
        "cargo test --locked",
        "cargo publish --locked -p agentic-server-core",
        "cargo publish --locked -p agentic-server",
    ):
        assert command in workflow
    assert "cargo package --no-verify --locked -p agentic-server" in normalized_block
    assert (
        "--config 'patch.crates-io.agentic-server-core.path=\"crates/agentic-server-core\"'" in normalized_block
    )
    assert "cargo package --list" not in normalized_block

    package = subprocess.run(
        [
            "cargo",
            "package",
            "--no-verify",
            "--locked",
            "--allow-dirty",
            "-p",
            "agentic-server",
            "--config",
            'patch.crates-io.agentic-server-core.path="crates/agentic-server-core"',
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    assert package.returncode == 0, package.stderr
    assert (REPO_ROOT / "target" / "package" / f"agentic-server-{WORKSPACE_VERSION}.crate").is_file()


def test_crate_release_duplicate_check_fails_closed_before_build(tmp_path: Path) -> None:
    import textwrap

    workflow = CRATE_RELEASE_WORKFLOW.read_text()
    assert workflow.index('name: Check published versions') < workflow.index('name: Verify lockfile')
    block = next(block for block in _workflow_run_blocks(workflow) if 'api/v1/crates/' in block)
    script = textwrap.dedent(block.removeprefix('|').lstrip('\n'))
    curl = tmp_path / 'curl'
    curl.write_text('#!/bin/sh\ncase "$*" in\n  *agentic-server-core*) printf "%s" "$CORE_STATUS" ;;\n  *) printf "%s" "$SERVER_STATUS" ;;\nesac\nexit "$TEST_CURL_EXIT"\n')
    curl.chmod(0o755)
    env = os.environ.copy()
    env.update(PATH=f'{tmp_path}:{env["PATH"]}', AGENTIC_RELEASE_VERSION=WORKSPACE_VERSION)
    cases = [('404', '404', '0', True), ('200', '404', '0', False), ('404', '200', '0', False),
             ('500', '404', '0', False), ('404', '429', '0', False), ('000', '404', '28', False)]
    for core, server, exit_code, succeeds in cases:
        env.update(CORE_STATUS=core, SERVER_STATUS=server, TEST_CURL_EXIT=exit_code)
        result = subprocess.run(['bash', '-eu', '-c', script], env=env, capture_output=True, text=True)
        assert (result.returncode == 0) == succeeds, (core, server, result.stdout, result.stderr)
        if '200' in (core, server):
            assert 'already published' in result.stdout + result.stderr


def test_python_workflows_pin_build_tools_and_manylinux_artifact_contract() -> None:
    release_workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    python_workflow = PYTHON_WORKFLOW.read_text(encoding="utf-8")
    constraints = BUILD_CONSTRAINTS.read_text(encoding="utf-8")

    assert "maturin==1.14.1" in constraints
    assert "pytest==9.1.1" in constraints
    assert "uv==0.11.21" in constraints
    assert "PyO3/maturin-action@86b9d133d34bc1b40018696f782949dac11bd380" in release_workflow
    assert (
        "quay.io/pypa/manylinux2014_x86_64@"
        "sha256:95440e0e72dd3a81dc8d2cf59a84d57af661456620f5bc821ff92048d0e54ff9"
    ) in release_workflow
    assert "manylinux: \"2014\"" in release_workflow
    assert "wheel-tag: py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64" in release_workflow
    assert "args: --release --locked" in release_workflow
    assert 'uv pip install --python .venv/bin/python "$wheel_path"' in release_workflow
    assert 'AGENTIC_API_TEST_WHEEL="$wheel_path" .venv/bin/python -m pytest tests/python -q' in release_workflow
    assert "AGENTIC_API_CHECK_PYTHON=.venv/bin/python" in release_workflow
    assert "AGENTIC_API_CHECK_SCRIPTS_DIR=.venv/bin" in release_workflow
    assert "hashFiles('Cargo.lock', 'python-build-constraints.txt')" in release_workflow
    assert "hashFiles('Cargo.lock', 'python-build-constraints.txt')" in python_workflow


def test_python_workflow_validates_crate_release_workflow_changes() -> None:
    workflow = PYTHON_WORKFLOW.read_text(encoding="utf-8")

    for trigger in ("pull_request", "push"):
        section = _workflow_trigger_section(workflow, trigger)
        assert section.count('".github/workflows/release-crates.yml"') == 1


def _workflow_run_blocks(workflow: str) -> list[str]:
    lines = workflow.splitlines()
    blocks: list[str] = []
    for index, line in enumerate(lines):
        stripped = line.lstrip()
        if not stripped.startswith("run:"):
            continue

        indent = len(line) - len(stripped)
        block = [stripped.removeprefix("run:").strip()]
        for candidate in lines[index + 1 :]:
            candidate_stripped = candidate.lstrip()
            candidate_indent = len(candidate) - len(candidate_stripped)
            if candidate_stripped and candidate_indent <= indent:
                break
            block.append(candidate)
        blocks.append("\n".join(block))
    return blocks


def _workflow_trigger_section(workflow: str, trigger: str) -> str:
    lines = workflow.splitlines()
    start = lines.index(f"  {trigger}:")
    section: list[str] = []
    for line in lines[start + 1 :]:
        if line.startswith("  ") and not line.startswith("    ") and line.strip():
            break
        section.append(line)
    return "\n".join(section)


def test_python_publishing_is_opt_in_and_waits_for_validated_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text()
    assert '      publish:\n' in workflow
    publish_input = workflow.split('      publish:\n', 1)[1].split('\nconcurrency:', 1)[0]
    assert 'type: boolean' in publish_input
    assert 'default: false' in publish_input
    build, publish = workflow.split('\n  publish:\n', 1)
    assert 'id-token: write' not in build
    assert 'needs: [release-version, build-wheels]' in publish
    assert "if: github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' && inputs.publish" in publish
    assert 'always()' not in publish
    assert 'name: pypi' in publish
    assert 'id-token: write' in publish
    assert 'actions/download-artifact@' in publish
    assert 'pattern: agentic-api-${{ needs.release-version.outputs.version }}-*' in publish
    assert 'merge-multiple: true' in publish
    assert 'pypa/gh-action-pypi-publish@' in publish
    assert 'packages-dir: dist/' in publish
    assert 'skip-existing: true' not in publish
    assert 'secrets.' not in publish
    assert 'actions/checkout@' not in publish
    assert publish.index('Validate publication artifacts') < publish.index('pypa/gh-action-pypi-publish@')


def test_publication_artifact_gate_requires_exact_wheel_set(tmp_path: Path) -> None:
    workflow = RELEASE_WORKFLOW.read_text()
    block = next(block for block in _workflow_run_blocks(workflow) if 'Expected exactly' in block)
    # Run the workflow's actual validation body without invoking an upload.
    import textwrap

    script = textwrap.dedent(block.removeprefix('|').lstrip('\n'))
    dist = tmp_path / 'dist'
    dist.mkdir()
    tags = (
        'py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64',
        'py3-none-macosx_10_12_x86_64',
        'py3-none-macosx_11_0_arm64',
    )
    env = os.environ.copy()
    env['AGENTIC_API_RELEASE_VERSION'] = WORKSPACE_VERSION

    def validate() -> subprocess.CompletedProcess[str]:
        return subprocess.run(['bash', '-eu', '-c', script], cwd=tmp_path, env=env, capture_output=True, text=True)

    assert validate().returncode != 0
    for tag in tags:
        (dist / f'agentic_api-{WORKSPACE_VERSION}-{tag}.whl').touch()
    assert validate().returncode == 0
    extra = dist / 'unexpected.whl'
    extra.touch()
    assert validate().returncode != 0
    extra.unlink()
    wheel = dist / f'agentic_api-{WORKSPACE_VERSION}-{tags[0]}.whl'
    wheel.rename(dist / f'agentic_api-0.0.0-{tags[0]}.whl')
    assert validate().returncode != 0


def test_pypi_duplicate_version_check_fails_closed(tmp_path: Path) -> None:
    import textwrap

    workflow = RELEASE_WORKFLOW.read_text()
    version_job = workflow.split('  build-wheels:', 1)[0]
    assert 'name: Reject an existing PyPI release\n        if: inputs.publish' in version_job
    block = next(block for block in _workflow_run_blocks(version_job) if 'pypi.org/pypi/' in block)
    script = textwrap.dedent(block.removeprefix('|').lstrip('\n'))
    curl = tmp_path / 'curl'
    curl.write_text('#!/bin/sh\nprintf "%s" "$TEST_HTTP_STATUS"\nexit "$TEST_CURL_EXIT"\n')
    curl.chmod(0o755)
    env = os.environ.copy()
    env.update(PATH=f'{tmp_path}:{env["PATH"]}', AGENTIC_API_RELEASE_VERSION=WORKSPACE_VERSION)
    for status, exit_code, succeeds in [('404', '0', True), ('200', '0', False), ('403', '0', False),
                                        ('429', '0', False), ('500', '0', False), ('000', '28', False)]:
        env.update(TEST_HTTP_STATUS=status, TEST_CURL_EXIT=exit_code)
        result = subprocess.run(['bash', '-eu', '-c', script], env=env, capture_output=True, text=True)
        assert (result.returncode == 0) == succeeds, (status, result.stdout, result.stderr)
        if status == '200':
            assert 'already published' in result.stdout + result.stderr



def test_release_installs_declared_rust_components_before_cargo() -> None:
    workflow = RELEASE_WORKFLOW.read_text()
    install = workflow.split('      - name: Install Rust toolchain', 1)[1].split('      - name:', 1)[0]
    assert 'components: clippy, rustfmt' in install
