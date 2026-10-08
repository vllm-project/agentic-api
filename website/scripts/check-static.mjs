import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { rewritePublishedLinks } from './refresh-site-docs.mjs';
const output = resolve(process.env.STATIC_OUTPUT_DIR || 'dist/client');
const basePath = process.env.NEXT_PUBLIC_BASE_PATH || '';
const siteUrl = process.env.NEXT_PUBLIC_SITE_URL;

const docs = JSON.parse(readFileSync('lib/data/docs-versions.json', 'utf8'));
const siteDocs = JSON.parse(readFileSync('lib/data/site-docs.json', 'utf8'));
assert.ok(Array.isArray(siteDocs.documents) && siteDocs.documents.length > 0);
const siteDocPages = new Map(
  siteDocs.documents.map((entry) => [`docs/latest/${entry.slug}.html`, entry]),
);
assert.ok(docs.versions.some((version) => version.id === docs.defaultVersion));
assert.equal(
  new Set(docs.versions.map((version) => version.id)).size,
  docs.versions.length,
);
for (const version of docs.versions) {
  assert.match(version.id, /^[a-z0-9][a-z0-9.-]*$/);
  if (version.channel === 'release')
    assert.match(
      version.sourceRef,
      /^[a-f0-9]{40}$/,
      'Release docs must pin an immutable source revision',
    );
}
const pages = [
  ['index.html', 'Your agent harness.', 'get-started'],
  ['community/team.html', 'behind the', 'Project Stewards'],
  [
    'community/contributors.html',
    'Every contribution.',
    'Meet the contributors.',
  ],
  ['404.html', 'A small detour.', 'Back to home'],
  ['roadmap.html', 'The road'],
  ['docs.html', 'Your version.'],
  ['docs/latest/python-cli.html', 'Python CLI.'],
  ['docs/latest/rust-cli.html', 'Rust CLI.'],
  ...docs.versions.map((version) => [
    `docs/${version.id}.html`,
    'Your version.',
  ]),
  ...[...siteDocPages].map(([file, entry]) => [
    file,
    entry.title.split('`')[0],
  ]),
];
const errors = [];
function resolvePage(path) {
  if (basePath && !path.startsWith(`${basePath}/`) && path !== basePath)
    return false;
  const base = resolve(output, '.' + (path.slice(basePath.length) || '/'));
  return [base, base + '.html', base + '/index.html'].some(existsSync);
}
for (const [file, heading] of pages) {
  const filename = resolve(output, file);
  if (!existsSync(filename)) {
    errors.push(`Missing static page: ${file}`);
    continue;
  }
  const html = readFileSync(filename, 'utf8');
  const document = html.replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, '');
  assert.equal(
    (document.match(/<h1\b/g) || []).length,
    1,
    `${file}: exactly one h1`,
  );
  assert.ok(document.includes(heading), `${file}: expected page content`);
  assert.ok(/<title>[^<]+<\/title>/.test(document), `${file}: document title`);
  assert.ok(/name="description"/.test(document), `${file}: page description`);
  assert.ok(/id="main"/.test(document), `${file}: skip-link target`);
  assert.ok(
    document.includes('rel="describedby"') &&
      document.includes('href="' + basePath + '/llms.txt"'),
    file + ': agent documentation discovery link includes deployment path',
  );
  if (file === 'index.html') {
    assert.ok(
      document.includes(
        'cargo install agentic-server --version 0.9.0 --locked',
      ),
      'The quickstart initially shows the published Cargo package',
    );
    assert.ok(
      document.includes('agentic run codex') &&
        !document.includes('./target/debug/agentic'),
      'The default launch instructions use the installed CLI',
    );
  }
  if (siteUrl && file !== '404.html' && file !== 'index.html') {
    const route = file === 'index.html' ? '/' : `/${file.slice(0, -5)}`;
    assert.ok(
      document.includes(`href="${siteUrl.replace(/\/$/, '')}${route}"`),
      `${file}: canonical URL includes deployment path`,
    );
  }
  if (
    file === 'roadmap.html' ||
    /docs\/latest\/(python|rust)-cli\.html/.test(file)
  ) {
    const article = document.match(
      /<article\b[^>]*>([\s\S]*?)<\/article>/,
    )?.[1];
    assert.ok(article, `${file}: Markdown content is present`);
    const headings = [
      ...article.matchAll(/<h[1-6]\b([^>]*)>([\s\S]*?)<\/h[1-6]>/g),
    ];
    assert.ok(
      headings.length >= 4,
      `${file}: documentation headings are rendered`,
    );
    const ids = headings.map(([, attributes, content]) => {
      const id = attributes.match(/\bid="([^"]+)"/)?.[1];
      assert.ok(id, `${file}: each documentation heading has a fragment ID`);
      assert.ok(
        content.includes(`href="#${id}"`),
        `${file}: heading links to its own fragment`,
      );
      assert.ok(
        content.includes('class="heading-anchor"'),
        `${file}: visible permalink`,
      );
      const link = content.match(
        /^(<a\b[^>]*class="heading-anchor"[^>]*>)([\s\S]+)<\/a>$/,
      );
      assert.ok(link, `${file}: the entire heading is a single link`);
      const [, permalink, label] = link;
      assert.ok(
        label.replace(/<[^>]*>/g, '').trim().length > 1 &&
          !permalink.includes('aria-label=') &&
          !label.includes('<a '),
        `${file}: heading text supplies the link name without nested links`,
      );
      assert.ok(
        !permalink.includes('tabindex="-1"') &&
          !permalink.includes('aria-hidden="true"'),
        `${file}: permalink is keyboard accessible`,
      );
      if (file !== 'roadmap.html')
        assert.ok(
          label.includes('class="command-prompt" aria-hidden="true">$</span>'),
          `${file}: decorative terminal prompt is inside the heading link`,
        );
      return id;
    });
    assert.equal(
      new Set(ids).size,
      ids.length,
      `${file}: unique heading destinations`,
    );
  }
  if (/docs\/latest\/(python|rust)-cli\.html/.test(file)) {
    const kind = file.includes('rust-cli') ? 'rust' : 'python';
    assert.ok(
      document.includes(
        `href="${basePath}/docs/latest/${kind}-cli" aria-current="page"`,
      ),
      `${file}: selected CLI`,
    );
    assert.ok(
      document.includes(`href="${basePath}/docs/latest/rust-cli"`) &&
        document.includes(`href="${basePath}/docs/latest/python-cli"`),
      `${file}: both references are linked`,
    );
    const expectedIds =
      kind === 'rust'
        ? [
            'agentic',
            'agentic-run-codex',
            'agentic-harness-claude',
            'agentic-serve',
            'agentic-validate',
          ]
        : [
            'agentic-api',
            'agentic-api-serve',
            'agentic-api-doctor',
            'agentic-api-version',
          ];
    for (const id of expectedIds)
      assert.ok(
        document.includes(`id="${id}"`),
        `${file}: stable ${id} fragment`,
      );
    assert.ok(document.includes('Latest / development'));
    for (const command of kind === 'rust'
      ? [
          'agentic run codex',
          'agentic run claude',
          'agentic harness codex',
          'agentic harness claude',
          'agentic serve',
          'agentic validate',
          '--gateway-port',
        ]
      : [
          'agentic-api serve',
          'agentic-api doctor',
          'agentic-api version',
          '--vllm-base-url',
          '--json',
        ])
      assert.ok(
        document.includes(command),
        `${kind} CLI reference includes ${command}`,
      );
  } else if (file === 'docs.html' || /^docs\/[^/]+\.html$/.test(file)) {
    const versionId =
      file === 'docs.html' ? docs.defaultVersion : file.slice(5, -5);
    const version = docs.versions.find((item) => item.id === versionId);
    const sourceLinks = [
      ...document.matchAll(
        /href="(https:\/\/github\.com\/vllm-project\/agentic-api\/(?:blob|tree)\/[^" ]+\/docs(?:\/[^" ]*)?)"/g,
      ),
    ].map((match) => match[1]);
    if (!version.hostedBaseUrl) {
      assert.ok(sourceLinks.length >= 2, `${file}: visible guide destinations`);
      assert.ok(
        sourceLinks.every((link) =>
          link.includes(`/${version.sourceRef}/docs`),
        ),
        `${file}: every guide stays in the selected version`,
      );
    }
    if (version.id === 'latest') {
      for (const entry of siteDocs.documents)
        if (version.sections.includes(entry.id))
          assert.ok(
            document.includes(`href="${basePath}/docs/latest/${entry.slug}"`),
            `${file}: ${entry.slug} opens its native latest page`,
          );
    } else {
      assert.equal(
        document.includes('/docs/guides/codex-desktop.md'),
        version.sections.includes('codex-desktop') && !version.hostedBaseUrl,
        `${file}: desktop guide follows the selected version's availability`,
      );
    }
    if (
      !version.sections.includes('sglang') &&
      !version.sections.includes('sglang-upstream')
    )
      assert.ok(
        !document.includes('SGLang upstream'),
        `${file}: unreleased guide is not advertised`,
      );
    if (version.channel === 'development')
      assert.ok(
        document.includes('Development documentation'),
        `${file}: development warning`,
      );
  }
  const siteDocument = siteDocPages.get(file);
  if (siteDocument) {
    assert.ok(
      document.includes('Development documentation'),
      `${file}: development warning`,
    );
    assert.ok(document.includes('Read as Markdown'), `${file}: Markdown link`);
    assert.ok(document.includes('View source'), `${file}: source link`);
    assert.ok(
      document.includes('class="roadmap-body"'),
      `${file}: rendered Markdown`,
    );
    assert.ok(
      document.includes(
        `href="${basePath}/docs/latest/${siteDocument.slug}.md"`,
      ),
      `${file}: Markdown export link`,
    );
  }
  for (const match of document.matchAll(
    /(?:href|src)="(\/[^"?#]*)(?:[?#][^"]*)?"/g,
  )) {
    if (!resolvePage(match[1]))
      errors.push(`${file}: missing local destination ${match[1]}`);
  }
}
for (const kind of ['python', 'rust']) {
  assert.equal(
    readFileSync(resolve(output, `docs/latest/${kind}-cli.md`), 'utf8'),
    readFileSync(`content/${kind}-cli.md`, 'utf8'),
    `${kind} CLI Markdown export matches the rendered reference`,
  );
}
for (const entry of siteDocs.documents) {
  const snapshot = readFileSync(`content/docs/${entry.sourcePath}`, 'utf8');
  const expected = rewritePublishedLinks(
    entry,
    snapshot,
    siteDocs.documents,
    siteUrl,
  );
  assert.equal(
    readFileSync(resolve(output, `docs/latest/${entry.slug}.md`), 'utf8'),
    expected,
    `${entry.slug}: published Markdown has current content and resolved links`,
  );
}
const llms = readFileSync(resolve(output, 'llms.txt'), 'utf8');
assert.equal(
  readFileSync(resolve(output, 'roadmap.md'), 'utf8'),
  readFileSync('content/roadmap.md', 'utf8'),
  'The published Markdown roadmap matches the source used to render the page',
);
assert.ok(llms.startsWith('# vLLM Agentic API\n\n> '), 'llms.txt overview');
for (const entry of siteDocs.documents)
  assert.ok(
    llms.includes(
      `https://vllm-project.github.io/agentic-api/docs/latest/${entry.slug}.md`,
    ),
    `llms.txt links to the ${entry.slug} Markdown export`,
  );
const manifest = JSON.parse(
  readFileSync('dist/server/vinext-prerender.json', 'utf8'),
);
assert.equal(
  manifest.routes.filter((route) => route.status !== 'rendered').length,
  0,
  'Every route must be statically rendered',
);
assert.deepEqual(errors, [], errors.join('\n'));
console.log(
  `Verified ${pages.length} static pages, metadata, local links, assets, and complete prerender output.`,
);
