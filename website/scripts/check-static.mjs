import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
const output = resolve('dist/client');
const docs = JSON.parse(readFileSync('lib/data/docs-versions.json', 'utf8'));
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
  ['community/contributors.html', 'Every contribution.', 'All contributors'],
  ['404.html', 'A small detour.', 'Back to home'],
  ['docs.html', 'Your version.'],
  ...docs.versions.map((version) => [
    `docs/${version.id}.html`,
    'Your version.',
  ]),
];
const errors = [];
function resolvePage(path) {
  const base = resolve(output, '.' + path);
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
  if (file.startsWith('docs')) {
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
    if (!version.sections.includes('sglang'))
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
  for (const match of document.matchAll(
    /(?:href|src)="(\/[^"?#]*)(?:[?#][^"]*)?"/g,
  )) {
    if (!resolvePage(match[1]))
      errors.push(`${file}: missing local destination ${match[1]}`);
  }
}
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
