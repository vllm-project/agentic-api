# Releasing Agentic API

Releases are deliberate maintainer actions. Merging a PR does **not** publish to PyPI or crates.io.
Relevant PRs, merge-queue entries, and pushes to `main` build and validate Python release wheels automatically;
a maintainer triggers the release workflow, and GitHub Actions handles validation and publication.

The workflow files are the source of truth:

| Workflow | Purpose | Default / inputs | Publishes |
| --- | --- | --- | --- |
| [Prepare release PR](https://github.com/vllm-project/agentic-api/actions/workflows/prepare-release-pr.yml) | Open the version-bump PR | Required `version` | Nothing |
| [Release crates](https://github.com/vllm-project/agentic-api/actions/workflows/release-crates.yml) | Validate or publish Rust packages | `dry_run=true`; required `version` | With `dry_run=false`: crates, Git tag, GitHub release; then publishes the tagged container and deploys the website |
| [Release container](https://github.com/vllm-project/agentic-api/actions/workflows/release-container.yml) | Publish a tagged release or nightly image | Called by crates release; also release events, daily schedule, or manual `source_ref` / optional `image_tag` | Docker Hub image; optional cleanup of old `nightly-*` tags |
| [Release Python](https://github.com/vllm-project/agentic-api/actions/workflows/release-python.yml) | Build and validate all release wheels | `publish=false`; no version input | With `publish=true`: PyPI wheels after all builds pass |

There is no automatic handoff from crates publishing to PyPI publishing. Dispatch both for a release of both
distributions. Crates and Python publication runs must use `main`; Python build-only validation can run on a branch.
Container releases build the release tag. Manual container runs select their source separately from the workflow ref.

## Credentials and permissions

- **Release preparation and crates:** the workflows require the triggering GitHub user to have `maintain` or `admin`
  repository permission. Repository Actions settings and branch/tag rules must allow the workflow to create its
  release branch, PR, tag, and GitHub release.
- **crates.io:** configure the repository Actions secret `CARGO_REGISTRY_TOKEN` with a crates.io token authorized to
  publish `vllm-responses`, `agentic-server-core`, and `agentic-server`. Publishing uses this token; a dry run does
  not upload packages.
- **PyPI:** configure a [Trusted Publisher](https://docs.pypi.org/trusted-publishers/adding-a-publisher/) on the
  `agentic-api` project with owner `vllm-project`, repository `agentic-api`, workflow filename `release-python.yml`,
  and environment `pypi`. The GitHub environment must also be named `pypi`; complete any configured environment
  approval before the publishing job can run. No PyPI API-token secret is used. Only the publishing job has
  `id-token: write` permission.
- **Docker Hub:** configure the `docker-publish` GitHub environment with variable `DOCKERHUB_IMAGE`
  (`namespace/repository`) and secrets `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN`. The credential needs image push
  access to that repository. Public dependencies are pulled anonymously before login; the token is used to push
  the finished image. To enable nightly cleanup, grant tag-read and tag-delete permissions and set environment
  variable `DOCKERHUB_CLEANUP_ENABLED` to `true`. Otherwise cleanup is skipped with a warning and 30-day retention
  is not enforced. Ensure environment branch/tag rules allow the intended workflow refs and complete any configured
  approval. Manual container publication also requires `maintain` or `admin`. The publishing job uses
  `GITHUB_TOKEN` with `statuses: write` to record successful nightly pushes without querying Docker Hub.

These are configuration requirements, not confirmation that a token or publisher is currently valid. A build-only
run verifies packaging and tests, but does not exercise registry upload authorization. TestPyPI is not configured.

## 1. Prepare the version

Choose an unused version. `[workspace.package].version` in `Cargo.toml` is the source of truth; the workspace's
`agentic-core` dependency version and workspace package entries in `Cargo.lock` must agree. Python declares a dynamic
version in `pyproject.toml`; Maturin derives it from the Rust package, and `agentic_api.__version__` reads installed
package metadata. Do not add a separate Python version or a fallback release version.

In GitHub, open **Actions → Prepare release PR → Run workflow**, select `main`, and enter the new `version` without
a `v` prefix. This updates `Cargo.toml` and `Cargo.lock`, runs Rust checks, and opens `release-prep/v<VERSION>` against
`main`. It fails if that branch already exists; inspect the existing PR before trying again.

The equivalent CLI command, from this repository, is:

```bash
# Example only: choose a version not already published.
release_version=0.8.0
gh workflow run prepare-release-pr.yml --ref main -f version="$release_version"
```

Review the diff and wait for required CI, including the three-platform Python release matrix. Automated PRs can
require a maintainer to select **Approve workflows to run** before CI starts; see
[GitHub's workflow-trigger rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).
If needed, dispatch Python validation on the preparation branch with `publish=false`.
The preparation workflow updates versions, not release notes. Move the applicable `CHANGELOG.md` entries from
`Unreleased` into a dated release section, include Rust API migration notes, and check that the selected version
reflects compatibility changes. Merge the preparation PR only after validation succeeds.

## 2. Validate the merged release

Record the intended commit SHA and declared version. Both publishing workflows resolve `main` when dispatched;
the crates version input checks the version, but does not pin the commit. Inspect each run's commit before proceeding.
If `main` advances between runs, review and validate the additional changes rather than assuming the artifacts match.

From the GitHub Actions UI:

1. Run **Release crates** on `main`, enter the declared `version`, and leave `dry_run` checked.
2. Run **Release Python** on `main` with `publish` unchecked, or inspect the completed automatic validation for the
   same commit.

CLI equivalents:

```bash
gh workflow run release-crates.yml --ref main -f version="$release_version" -F dry_run=true
gh workflow run release-python.yml --ref main -F publish=false
```

The crates dry run checks versions, unused release refs, registry availability, lockfile consistency, formatting,
Clippy, and tests. It dry-runs publication of `vllm-responses`, then core with a local `vllm-responses` override, and
then server with local core and `vllm-responses` overrides. It does not prove registry authentication.

Python validation builds these exact wheel variants, installs each wheel, and runs the Python tests and wheel checks:

| Platform | Wheel suffix |
| --- | --- |
| Linux x86_64 | `py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl` |
| macOS Intel | `py3-none-macosx_10_12_x86_64.whl` |
| macOS Apple Silicon | `py3-none-macosx_11_0_arm64.whl` |

Linux uses the pinned manylinux image with vendored OpenSSL and its Perl build prerequisites. The installed binary
must have no unresolved shared libraries or system `libssl`/`libcrypto` dependency. Keep this release matrix in CI
when changing packaging or toolchain configuration; a successful host build alone does not establish portability.

`pyproject.toml` points to `python/README.md` for the PyPI description. Review that README and project URLs before
publishing. The installed metadata test rejects an empty or non-Markdown description. To check downloaded artifacts
for rendering errors too, run `uvx --from twine twine check --strict /path/to/wheels/*.whl`.

## 3. Trigger the publishing workflows

After validation, use **Actions → Release crates → Run workflow** on `main`, enter the same version, and **uncheck
`dry_run`**. The workflow publishes `vllm-responses`, waits for it to appear in the crates.io index, publishes core,
waits for core, then publishes the server, creates `v<VERSION>` and its GitHub release, and calls the container and
website workflows. The container job builds that
Git tag and pushes `DOCKERHUB_IMAGE:v<VERSION>`; it does not add a `latest` tag. Rust publication currently includes
`vllm-responses`, `agentic-server-core`, and `agentic-server`.

The explicit reusable container call is required because a release created with `GITHUB_TOKEN` does not trigger
another release-event workflow. Separately published GitHub releases can trigger the container workflow directly.
A crates dry run skips both container publication and website deployment.

Then use **Actions → Release Python → Run workflow** on `main` and **check `publish`**. There is no version field;
it reads the declaration in `Cargo.toml`. It checks that the version is unused, rebuilds and validates all three
wheels, then uploads exactly those artifacts from the same run. A successful earlier build-only run is not promoted
or reused. The Python workflow does not create the Git tag or GitHub release.

Optional CLI equivalents to trigger the same GitHub Actions workflows:

```bash
gh workflow run release-crates.yml --ref main -f version="$release_version" -F dry_run=false
gh workflow run release-python.yml --ref main -F publish=true
```

Trigger each publishing workflow once, following each run to completion before moving on. If one registry already
has the intended release, verify it and trigger only the workflow for the remaining registry. Do not rerun the
completed publication.

## 4. Verify availability and update install instructions

A green build job alone is not a release. Check the publishing job and the registry contents:

- All three crates are available at the intended version, and the `v<VERSION>` tag/GitHub release point to the expected SHA.
- The PyPI version has all three wheels listed above and the expected project description and links.
- The container publishing job succeeded and the expected Docker Hub tag exists. Pull it and inspect the
  `org.opencontainers.image.revision` and `org.opencontainers.image.version` labels against the intended SHA and
  release image tag (`v<VERSION>`). Record its digest for deployment; the build-only container CI does not prove publication.
- Install from the registries in a clean environment and check the reported version:

```bash
uvx --from "agentic-api==$release_version" agentic --version
cargo install agentic-server --version "=$release_version" --locked --root /tmp/agentic-release-smoke
/tmp/agentic-release-smoke/bin/agentic-server --version
```

After publication is confirmed, update `PUBLISHED_VERSION` in `website/lib/quickstart.ts`, the matching install
examples in `website/public/llms.txt`, `website/README.md`, and `docs/guides/python-installation.md`, plus any pinned
README examples and static website assertions. Keep `python/README.md` accurate for the next Python release.
Also update the documentation version manifest in `website/lib/data/docs-versions.json`: resolve the release tag
to its full commit SHA, verify the listed guide paths at that commit, and set the new default version and check date
as described in `website/docs/versioning.md`. The crates workflow's website deployment does not edit these files
or prove PyPI availability. Merge the website changes and verify deployment before announcing the updated install
instructions.

## Nightly containers and container-only recovery

The container workflow runs daily at 05:00 UTC, building `main` and publishing both `nightly-<full-SHA>` and `nightly`.
A manual run whose selected commit equals current `main` also publishes those tags, plus `image_tag` if supplied.
If the same commit has a successful publication status for the same Docker Hub repository, the nightly run skips
the build, login, and all pushes, including a manual `image_tag` override. Success is recorded after every tag has
been pushed and before optional cleanup. The first run may publish a previously built commit once because older
publications have no status record. A GitHub status lookup failure stops the run.

Other manual refs use the explicit `image_tag`, or the source ref with slashes replaced by hyphens. When explicitly
enabled, cleanup removes `nightly-*` tags last updated more than 30 days ago, even on runs that skip a duplicate
nightly build; stable release tags are outside that cleanup.

If crates and the GitHub release succeeded but the container job failed, inspect the Docker Hub tag and run logs
first. The image may already have been pushed before nightly cleanup failed. Repair credentials, environment rules,
or the failed step as appropriate. To rebuild just the image from the verified release tag, run the container
workflow from `main` with an explicit source and image tag:

```bash
gh workflow run release-container.yml --ref main \
  -f source_ref="refs/tags/v$release_version" -f image_tag="v$release_version"
```

This publishes an image; it has no dry-run mode and does not republish crates or PyPI wheels. Confirm the existing
Git tag resolves to the intended release SHA before dispatching. A manual rebuild can replace the Docker tag's
existing digest, so verify and record the new digest and labels afterward. If that commit is still current `main`,
the manual run also follows the nightly rules above and may skip all pushes when that commit was already published.
For a failed release container job, rerunning only that job preserves explicit release mode and avoids this nightly
skip. If recording a nightly success status fails after a push, inspect registry state before retrying: the Docker
push and GitHub status update are separate operations. Container publication currently uses the default
Linux runner architecture; do not assume a multi-platform image from the Python wheel matrix.

## Failed runs and duplicate versions

- **Build or test failure before upload:** fix it through a PR and rerun validation. Record the new commit; do not
  bypass checks to publish artifacts from a failed matrix.
- **Duplicate version:** publication must fail. Python rejects an existing PyPI version before building. Crates
  rejects an existing tag or either published crate version, including during dry runs. Python build-only runs can
  still validate an already published version. Never add `skip-existing` or remove these guards to make a run green.
- **Registry lookup failure:** only HTTP 404 counts as unused. Timeouts and other HTTP responses fail the run; they
  are not permission to publish.
- **Partial upload:** inspect the registry, job logs, uploaded artifacts, and source SHA first. A rerun can fail
  because one wheel or the core crate already exists. The workflows do not automatically resume partial releases;
  decide on a reviewed recovery or a new version instead of blindly redispatching or deleting registry files.
- **Tag, GitHub release, container, or website failure after crate upload:** the crates may already be published. Repair the
  remaining step after checking the registry and commit; do not restart package publication just to deploy the site
  or rebuild the container. Use the container-only recovery above when the release tag already exists.
- **Description or metadata correction after upload:** fix `pyproject.toml` or `python/README.md` and publish a new
  version. Existing wheel filenames cannot be replaced. Do not describe a merged metadata fix as live on PyPI.
