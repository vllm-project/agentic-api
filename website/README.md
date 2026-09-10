# vLLM Agentic API website

A static, responsive website for the vLLM Agentic API project. Built with Vinext (React + TypeScript), Tailwind CSS, and Shadcn/Base UI primitives.

## Develop

```sh
npm ci
npm run dev
```

## Build and check

```sh
npm run build
npm run check
npm run format:check
```

Requires Node.js 22.13 or newer. The site exports static HTML and browser assets to `dist/client`. Deploy that folder with clean-URL HTML routing (`/docs/v0.5.0` serves `docs/v0.5.0.html`) and `404.html` for missing routes. `NEXT_PUBLIC_SITE_URL` and `NEXT_PUBLIC_BASE_PATH` configure the canonical URL and deployment prefix at build time; their defaults preserve the Sites preview. The PR preview is hosted at [vLLM Agentic API](https://vllm-agentic-api.franciscojavierarceo.chatgpt.site/). Hosting credentials and account configuration are not part of this repository. There is no application server, database, or runtime GitHub dependency.

Routes:

- `/` — project positioning, architecture, capabilities, Codex/Claude Code examples, and community links.
- `/community/team` — maintainers from the project's CODEOWNERS.
- `/community/contributors` — a contributor directory and guidance for thoughtful contributions.
- `/docs` — documentation directory for the default tagged version.
- `/docs/latest` and `/docs/v0.1.0` through `/docs/v0.5.0` — shareable documentation directories for development and tagged snapshots.

## GitHub Pages deployment

The Website workflow validates pull requests and deploys changes under `website/` on `main`. Each deployment checks out current `main` and refreshes contributors before building, checking, and uploading the static artifact, so a long-running release cannot roll back newer website changes. Refreshed data is included in that deployment without committing generated changes back to the repository. The Release crates workflow calls the same workflow after a successful publication; dry runs and failed releases do not deploy. This explicit call is needed because releases created with `GITHUB_TOKEN` do not trigger another release event workflow. You can also redeploy from `main` with the Website workflow’s **Run workflow** button.

After this PR merges, switch **Settings → Pages → Build and deployment → Source** from the current `main` / `/docs` branch source to **GitHub Actions**, then run the Website workflow on `main`. Keep the `github-pages` environment restricted to `main`. No personal access token or new repository secret is needed. The existing Pages site remains unchanged until that first deployment.

The workflow targets `https://vllm-project.github.io/agentic-api`. `npm run prepare:pages` packages Vinext’s prefixed output into `dist/pages`, with directory index files for nested routes and `.nojekyll` for static assets. To reproduce the Pages build locally:

```sh
export NEXT_PUBLIC_BASE_PATH=/agentic-api
export NEXT_PUBLIC_SITE_URL=https://vllm-project.github.io/agentic-api
export STATIC_OUTPUT_DIR=dist/pages
npm test
npm run refresh:contributors
npm run build
npm run prepare:pages
npm run check
```

Update the two URL variables in the workflow when moving to a custom domain.

## Content

- `lib/site.ts`: project and documentation links.
- `lib/data/docs-versions.json`: available documentation versions, the default version, pinned source revisions, and optional hosted documentation destinations.
- `lib/docs.ts`: documentation sections and version-specific link resolution.
- `lib/quickstart.ts`: visible CLI examples and the optional read-only WebMCP tool contract.
- `lib/data/community.json`: bundled profile snapshot, contributor refresh date, and roster sources; contributor profiles retain GitHub’s contribution order.
- `public/people/`: public GitHub avatars, bundled to avoid runtime requests.

Documentation navigation opens the site's version directory. The project's configured Read the Docs site returned HTTP 404 on September 9, 2026, so guides currently open on GitHub. Tagged documentation links are pinned to the tag's resolved commit, while `latest` intentionally tracks `main`. The newer SGLang, Dynamo, and agentic-llm-d pages are shown only for development because those files are absent from the checked release snapshots. See [documentation versioning](docs/versioning.md) for adding versions and connecting the existing MkDocs/Read the Docs setup when it is available.

The team roster follows [CODEOWNERS](https://github.com/vllm-project/agentic-api/blob/main/.github/CODEOWNERS). Contributor profiles come from [GitHub's contributors endpoint](https://api.github.com/repos/vllm-project/agentic-api/contributors?per_page=100). Run `npm run refresh:contributors` to fetch every page, refresh public display names and bundled photos, and update the displayed date. Set `GH_TOKEN` or `GITHUB_TOKEN` for authenticated API limits. Bots and anonymous entries are excluded from the people directory. Profiles retain descending contribution order, preserving source order for ties; individual counts and rank fields are discarded before the snapshot is written. The team roster is not changed.

Every profile uses the same presentation, including in the homepage avatar mosaic. This commit-based source is not a complete record of reviews, issue reports, or community support. GitHub caches the endpoint, so a new commit can take a few hours to appear. All API requests and photo downloads must succeed before the saved data is replaced. A failed refresh stops deployment and leaves the currently published site in place. Ordinary local builds and pull request checks use the bundled snapshot without contacting GitHub.

Integration copy and commands were checked against the project's [README](https://github.com/vllm-project/agentic-api/blob/main/README.md). Shell/editor function tools execute in the client. Built-in web search and MCP tools execute on the gateway. Model compatibility depends on the served model and its tool-calling configuration.

The community information architecture is inspired by the supplied vLLM Semantic Router pages. The layout, visual system, and wording were created for this site. The website does not run an agent or contact a model: the launch instructions are examples for a user's own environment.

## Branding

The site uses the project's official blue (`#30A2FF`) and gold (`#FDB515`). The logo, diagram brandmark, and favicon are sourced from `assets/White-Main-Logo.svg` and `assets/Main-Brandmark.svg` in the upstream repository and are bundled without artwork changes. Blue identifies links and interactive accents; gold highlights primary actions and key project labels.

## Inference ecosystem

The landing page links to the upstream [SGLang guide](https://github.com/vllm-project/agentic-api/blob/main/docs/guides/sglang-upstream.md), [NVIDIA Dynamo guide](https://github.com/vllm-project/agentic-api/blob/main/docs/guides/dynamo-upstream.md), and [agentic-llm-d design](https://github.com/vllm-project/agentic-api/blob/main/docs/design/agentic-llm-d.md). The named SGLang and Dynamo configurations are the documented recording baselines; the website describes recorded integration coverage without claiming every model or transport has been verified. The llm-d callout describes the separate v1alpha hydration/persistence service and its distinction from gateway-executed built-in tools.
