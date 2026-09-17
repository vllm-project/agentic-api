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

Requires Node.js 22.13 or newer plus Python 3.11+ and Rust/Cargo for source-based CLI documentation generation (CI uses Python 3.12 and Rust 1.98.0). Set `PYTHON` to a Python executable path if needed. Standalone website checkouts use bundled CLI references and do not need Python or Cargo. The site exports static HTML and browser assets to `dist/client`. Deploy that folder with clean-URL HTML routing (`/docs/v0.5.0` serves `docs/v0.5.0.html`) and `404.html` for missing routes. `NEXT_PUBLIC_SITE_URL` and `NEXT_PUBLIC_BASE_PATH` configure the canonical URL and deployment prefix at build time; their defaults preserve the Sites preview. The PR preview is hosted at [vLLM Agentic API](https://vllm-agentic-api.franciscojavierarceo.chatgpt.site/). Hosting credentials and account configuration are not part of this repository. There is no application server, database, or runtime GitHub dependency.

Routes:

- `/` — project positioning, architecture, capabilities, Codex/Claude Code examples, and community links.
- `/roadmap` — the repository roadmap, rendered as a full page; `/roadmap.md` provides the same source as Markdown.
- `/community/team` — maintainers from the project's CODEOWNERS.
- `/community/contributors` — a contributor directory and guidance for thoughtful contributions.
- `/docs` — documentation directory for the default tagged version.
- `/docs/latest` and `/docs/v0.1.0` through `/docs/v0.5.0` — shareable documentation directories for development and tagged snapshots.
- `/docs/latest/rust-cli` — Rust `agentic` command reference, generated from the Clap definitions; `/docs/latest/rust-cli.md` provides the same content as Markdown.
- `/docs/latest/python-cli` — Python launcher reference generated from the real CLI parser; `/docs/latest/python-cli.md` provides the same content as Markdown.
- `/llms.txt` — a concise Markdown project overview and curated links to raw documentation, following [llmstxt.org](https://llmstxt.org/).

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

The Python CLI reference is generated from `python/agentic_api/cli.py` before every development or production build by `scripts/prepare-content.mjs`. Edit the parser's help text to document a command or flag. The root `scripts/generate_python_cli_docs.py` walks the parser and renders help with defaults; it reads the workspace version from `Cargo.toml` and creates temporary package metadata so it can import the source without building a wheel, compiling Rust, or installing vLLM. It never executes a CLI command. The Website workflow also runs for Python source, generator, and workspace-version changes.

The Rust reference comes from the private `agentic-cli-docs` workspace tool, which compiles the real `agentic_cli.rs` and `agentic_output.rs` modules with only Clap and URL dependencies. It renders the command tree without running a gateway, harness, or model. No application executable or package entry point changes. The tool inherits workspace dependency versions and uses Cargo.lock. It includes all visible nested commands and suppresses environment-variable values in help output. The Website workflow tests the generator and also triggers on Rust command-definition, generator, and Cargo.lock changes. MkDocs and Read the Docs run the same generator during builds.

`content/python-cli.md` and `content/rust-cli.md` are the bundled snapshots used by standalone website previews. Full repository builds regenerate it; a generator failure stops the build. The Markdown export is generated under `public/docs/latest/` and linked from `llms.txt`. This is explicitly development documentation, separate from immutable tagged docs. MkDocs uses the same generator through its `on_pre_build` hook, writing the ignored `docs/reference/python-cli.md` and `docs/reference/rust-cli.md`; future versioned MkDocs builds generate help from their own checkout. Run `python3 scripts/generate_python_cli_docs.py` at the repository root to generate the Python MkDocs file manually; `cargo run --quiet --locked -p agentic-cli-docs` emits the Rust reference on stdout.

The roadmap page renders `ROADMAP.md` with `react-markdown`. The roadmap and both CLI references use the shared `LinkedMarkdown` renderer, with GitHub-style heading IDs and keyboard-accessible links covering each full heading. CLI headings include a decorative, non-selectable `$` marker inside the link but outside the heading slug, preserving existing fragment URLs. The CLI selector links between the two command references. Heading links work in static HTML without JavaScript; the existing scroll padding keeps destinations below the fixed header. Before development or production builds, `scripts/refresh-roadmap.mjs` copies the root roadmap to `content/roadmap.md` and generates `public/roadmap.md`. Standalone website checkouts use the bundled content snapshot. Edit the root roadmap in this repository; the Website workflow also runs for `ROADMAP.md` changes, so merging a roadmap update republishes the page. The website does not fetch the document at runtime.

- `lib/site.ts`: project and documentation links.
- `lib/data/docs-versions.json`: available documentation versions, the default version, pinned source revisions, and optional hosted documentation destinations.
- `lib/docs.ts`: documentation sections and version-specific link resolution.
- `lib/quickstart.ts`: visible CLI examples and the optional read-only WebMCP tool contract.
- `public/llms.txt`: agent-readable documentation index, copied into every static deployment. Each page links to it with `rel="describedby"` using the deployment base path.
- `lib/data/community.json`: bundled profile snapshot, contributor refresh date, and roster sources; contributor profiles retain GitHub’s contribution order.
- `public/people/`: public GitHub avatars, bundled to avoid runtime requests.

Documentation navigation opens the site's version directory. The project's configured Read the Docs site returned HTTP 404 on September 9, 2026, so guides currently open on GitHub. Tagged documentation links are pinned to the tag's resolved commit, while `latest` intentionally tracks `main`. The newer SGLang, Dynamo, and agentic-llm-d pages are shown only for development because those files are absent from the checked release snapshots. See [documentation versioning](docs/versioning.md) for adding versions and connecting the existing MkDocs/Read the Docs setup when it is available.

The team roster follows [CODEOWNERS](https://github.com/vllm-project/agentic-api/blob/main/.github/CODEOWNERS). Contributor profiles come from [GitHub's contributors endpoint](https://api.github.com/repos/vllm-project/agentic-api/contributors?per_page=100). Run `npm run refresh:contributors` to fetch every page, refresh public display names and bundled photos, and update the displayed date. Set `GH_TOKEN` or `GITHUB_TOKEN` for authenticated API limits. Bots and anonymous entries are excluded from the people directory. Profiles retain descending contribution order, preserving source order for ties; individual counts and rank fields are discarded before the snapshot is written. Team cards use this same contribution order at build time, without visible ranks or counts. Maintainers absent from the contributor list appear last in their existing roster order. The team roster membership is not changed.

Every profile uses the same presentation, including in the homepage avatar mosaic. This commit-based source is not a complete record of reviews, issue reports, or community support. GitHub caches the endpoint, so a new commit can take a few hours to appear. All API requests and photo downloads must succeed before the saved data is replaced. A failed refresh stops deployment and leaves the currently published site in place. Ordinary local builds and pull request checks use the bundled snapshot without contacting GitHub.

Integration copy and commands were checked against the project's [README](https://github.com/vllm-project/agentic-api/blob/main/README.md). Shell/editor function tools execute in the client. Built-in web search and MCP tools execute on the gateway. Model compatibility depends on the served model and its tool-calling configuration.

The quickstart also links to the [Codex Desktop guide](../docs/guides/codex-desktop.md), with the verified Linux setup and freeform `apply_patch` limitation. The guide appears in `/docs/latest` and `/llms.txt`; tagged documentation snapshots omit it until their source contains the guide.

The quickstart defaults to the published `agentic-server` crate and offers PyPI and build-from-source tabs. Both registry options pin version 0.7.0. PyPI uses `uvx --from agentic-api==0.7.0 agentic` for version checks, serving, and harness launches without a global installation. Cargo installs launch `agentic`; source builds launch `./target/debug/agentic`. The optional WebMCP tool accepts an `installation` value (`crates`, `pypi`, or `source`) and defaults to crates.io. `PUBLISHED_VERSION` in `lib/quickstart.ts` supplies the install commands and visible release labels; update it and `public/llms.txt` after publishing a new release. The base Python wheel bundles the Rust executables and does not install vLLM.

The community information architecture is inspired by the supplied vLLM Semantic Router pages. The layout, visual system, and wording were created for this site. The website does not run an agent or contact a model: the launch instructions are examples for a user's own environment.

## Branding

The site uses the project's official blue (`#30A2FF`) and gold (`#FDB515`). The logo, diagram brandmark, and favicon are sourced from `assets/White-Main-Logo.svg` and `assets/Main-Brandmark.svg` in the upstream repository and are bundled without artwork changes. Blue identifies links and interactive accents; gold highlights primary actions and key project labels. The header theme control offers Light, Dark, and System. System is the default and follows live system preferences through CSS `color-scheme` and `light-dark()`. Explicit choices are saved locally and applied before the page paints; selecting System clears the override. Theme switching also works for the current page when browser storage is unavailable. Light mode uses the unchanged `assets/Black-Main-Logo.svg` and darker blue/gold text tones for contrast; the original brand colors remain on logos and filled accents. Tailwind’s dark variants and both header/footer logos follow the selected theme. The community sections and footer link to the official vLLM Slack join page and name the `#sig-agentic-api` channel.

The effects stylesheet adds blue/gold lighting behind hero sections, brief entrance and diagram signal animations, and matching hover/focus highlights on cards. A small client controller moves the hero spotlight and nearby card reflections with the pointer, batching geometry reads and style updates once per animation frame. Tracking runs only with a fine, hover-capable pointer and no reduced-motion preference; touch, reduced motion, and unavailable JavaScript retain static lighting. Preference changes and route navigation clean up listeners, pending frames, and inline positions. Automatic entrance motion finishes within four seconds; hover interactions replay one short diagram cycle. The effects add no dependencies, and content stays visible without animation support.

## Inference ecosystem

The landing page links to the upstream [SGLang guide](https://github.com/vllm-project/agentic-api/blob/main/docs/guides/sglang-upstream.md), [NVIDIA Dynamo guide](https://github.com/vllm-project/agentic-api/blob/main/docs/guides/dynamo-upstream.md), and [agentic-llm-d design](https://github.com/vllm-project/agentic-api/blob/main/docs/design/agentic-llm-d.md). The named SGLang and Dynamo configurations are the documented recording baselines; the website describes recorded integration coverage without claiming every model or transport has been verified. The llm-d callout describes the separate v1alpha hydration/persistence service and its distinction from gateway-executed built-in tools.
