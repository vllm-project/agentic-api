# Documentation versioning

The marketing site and documentation content have separate publishing lifecycles. The site preserves the upstream MkDocs Material stack and Read the Docs configuration; it does not duplicate or rebuild the documentation source.

## Current behavior

- `/docs` displays the version selected by `defaultVersion` in `lib/data/docs-versions.json`.
- `/docs/<version>` is a static, shareable directory for that specific version.
- `/docs/latest` tracks development on `main`. Its notice makes clear that features may be unreleased.
- Tagged versions point to immutable source commits. Their links never silently switch to `main`.
- The picker and the plain version links use the same manifest. The links also work without JavaScript.
- Only sections verified to exist in that snapshot are listed. All other docs remain accessible through “Browse all”.

The initial snapshot was checked against the [upstream tags](https://github.com/vllm-project/agentic-api/tags) on September 9, 2026. A tag's presence does not imply an upstream support or maintenance guarantee. The `sourceRef` fields record the resolved tag commits; v0.2.0 and v0.3.0 currently resolve to the same commit.

## Add a release

1. Verify the upstream tag and its full commit SHA. Keep existing entries unchanged.
2. Add an entry to `lib/data/docs-versions.json` with a unique route-safe `id`, label, `channel: "release"`, tag in `ref`, and the full SHA in `sourceRef`.
3. Check each section's path in that commit. Add only available IDs from `lib/docs.ts` to `sections`. Leave `hostedBaseUrl` null while serving links to GitHub.
4. Update `defaultVersion` only when the new tag is intended to be the default. Keep development and prereleases distinct from the default release.
5. Update `checkedAt`, then run the build and `node scripts/check-static.mjs`. The dynamic route statically exports every manifest entry automatically.
6. Publish the updated website to its static host. Existing version URLs remain valid.

## Connect hosted documentation

The upstream repository already contains `mkdocs.yaml`, `.readthedocs.yaml`, and `docs/requirements.txt`. The configured `https://agentic-api.readthedocs.io/` endpoint returned 404 during this update, so no links to unavailable builds are exposed.

[Read the Docs versioning](https://docs.readthedocs.com/platform/stable/versions.html) supports active builds from tags and branches, a `latest` development version, and a `stable` alias for the greatest stable semantic version. It also provides a docs-site version menu and configurable version notices.

When the upstream project is connected and builds are available:

1. Activate the intended tag versions in Read the Docs and successfully build them. Preserve historical builds whose URLs have been shared.
2. Enable its version selector and the development/outdated-version notices. Set the default version to `stable` when that matches the project's release policy.
3. Verify the actual version URL slugs and guide paths. Set each manifest entry's `hostedBaseUrl` to the corresponding successful build, for example `https://agentic-api.readthedocs.io/en/v0.5.0/` **only after it exists**. Use the exact version for release entries; reserve `/en/latest/` for development. Do not point historical entries at `/en/stable/` because that alias moves.
4. Confirm the hosted content corresponds to the entry's recorded source revision. Rebuild and publish the site. Guide links will use MkDocs directory-style URLs and the GitHub notice disappears for those entries.

This update configures the website's version navigation, not the upstream Read the Docs account or release automation. Activating hosted builds remains a separate upstream setup step.
