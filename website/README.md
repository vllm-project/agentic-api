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

Requires Node.js 22.13 or newer. The site exports static HTML and browser assets to `dist/client`. Deploy that folder with clean-URL HTML routing (`/docs/v0.5.0` serves `docs/v0.5.0.html`) and `404.html` for missing routes. `lib/site.ts` holds the canonical site origin; update it when moving to a project-owned domain. The PR preview is hosted at [vLLM Agentic API](https://vllm-agentic-api.franciscojavierarceo.chatgpt.site/). Hosting credentials and account configuration are not part of this repository. There is no application server, database, or runtime GitHub dependency.

Routes:

- `/` — project positioning, architecture, capabilities, Codex/Claude Code examples, and community links.
- `/community/team` — maintainers from the project's CODEOWNERS.
- `/community/contributors` — a contributor directory and guidance for thoughtful contributions.
- `/docs` — documentation directory for the default tagged version.
- `/docs/latest` and `/docs/v0.1.0` through `/docs/v0.5.0` — shareable documentation directories for development and tagged snapshots.

## Content

- `lib/site.ts`: project and documentation links.
- `lib/data/docs-versions.json`: available documentation versions, the default version, pinned source revisions, and optional hosted documentation destinations.
- `lib/docs.ts`: documentation sections and version-specific link resolution.
- `lib/quickstart.ts`: visible CLI examples and the optional read-only WebMCP tool contract.
- `lib/data/community.json`: public names, handles, and roster sources checked September 9, 2026; contributor profiles retain GitHub’s contribution order.
- `public/people/`: public GitHub avatars, bundled to avoid runtime requests.

Documentation navigation opens the site's version directory. The project's configured Read the Docs site returned HTTP 404 on September 9, 2026, so guides currently open on GitHub. Tagged documentation links are pinned to the tag's resolved commit, while `latest` intentionally tracks `main`. The newer SGLang, Dynamo, and agentic-llm-d pages are shown only for development because those files are absent from the checked release snapshots. See [documentation versioning](docs/versioning.md) for adding versions and connecting the existing MkDocs/Read the Docs setup when it is available.

The team roster follows [CODEOWNERS](https://github.com/vllm-project/agentic-api/blob/main/.github/CODEOWNERS). Contributor names come from [GitHub's contributors endpoint](https://api.github.com/repos/vllm-project/agentic-api/contributors?per_page=100). When refreshing the list, order profiles by descending GitHub contribution count, preserving source order for ties, then retain only profile information. Individual commit totals, rank fields, and shares are not part of the website data, and the page does not label the ordering. Every profile uses the same presentation, including in the homepage avatar mosaic. The directory acknowledges that this source is not a complete record of documentation, reviews, issue reports, or community support.

Integration copy and commands were checked against the project's [README](https://github.com/vllm-project/agentic-api/blob/main/README.md). Shell/editor function tools execute in the client. Built-in web search and MCP tools execute on the gateway. Model compatibility depends on the served model and its tool-calling configuration.

The community information architecture is inspired by the supplied vLLM Semantic Router pages. The layout, visual system, and wording were created for this site. The website does not run an agent or contact a model: the launch instructions are examples for a user's own environment.

## Branding

The site uses the project's official blue (`#30A2FF`) and gold (`#FDB515`). The logo, diagram brandmark, and favicon are sourced from `assets/White-Main-Logo.svg` and `assets/Main-Brandmark.svg` in the upstream repository and are bundled without artwork changes. Blue identifies links and interactive accents; gold highlights primary actions and key project labels.

## Inference ecosystem

The landing page links to the upstream [SGLang guide](https://github.com/vllm-project/agentic-api/blob/main/docs/guides/sglang-upstream.md), [NVIDIA Dynamo guide](https://github.com/vllm-project/agentic-api/blob/main/docs/guides/dynamo-upstream.md), and [agentic-llm-d design](https://github.com/vllm-project/agentic-api/blob/main/docs/design/agentic-llm-d.md). The named SGLang and Dynamo configurations are the documented recording baselines; the website describes recorded integration coverage without claiming every model or transport has been verified. The llm-d callout describes the separate v1alpha hydration/persistence service and its distinction from gateway-executed built-in tools.
