import assert from 'node:assert/strict';
import { access, mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, posix, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const DEFAULT_SITE_URL = 'https://vllm-project.github.io/agentic-api';
const REPO_BLOB = 'https://github.com/vllm-project/agentic-api/blob/main/';

function isRelativeLink(url) {
  return (
    url &&
    !url.startsWith('#') &&
    !url.startsWith('/') &&
    !/^[a-z][a-z\d+.-]*:/i.test(url)
  );
}

function mapMarkdownLinks(markdown, transform) {
  let fence = null;
  return markdown
    .split(/(?<=\n)/)
    .map((line) => {
      const marker = /^ {0,3}(\x60{3,}|~{3,})/.exec(line)?.[1];
      if (marker) {
        if (!fence) fence = marker;
        else if (marker[0] === fence[0] && marker.length >= fence.length)
          fence = null;
        return line;
      }
      if (fence) return line;
      return line
        .replace(
          /(\]\(\s*<?)([^>\s)]+)/g,
          (_, prefix, url) => prefix + transform(url),
        )
        .replace(
          /^(\s*\[[^\]]+\]:\s*<?)([^>\s]+)/,
          (_, prefix, url) => prefix + transform(url),
        );
    })
    .join('');
}

export function rewritePublishedLinks(
  document,
  markdown,
  documents,
  siteUrl = process.env.NEXT_PUBLIC_SITE_URL || DEFAULT_SITE_URL,
) {
  const bySourcePath = new Map(
    documents.map((item) => ['docs/' + item.sourcePath, item]),
  );
  const siteBase = siteUrl.replace(/\/$/, '');
  const directory = posix.dirname('docs/' + document.sourcePath);
  const published = mapMarkdownLinks(markdown, (url) => {
    if (!isRelativeLink(url)) return url;
    const suffixAt = url.search(/[?#]/);
    const path = suffixAt < 0 ? url : url.slice(0, suffixAt);
    const suffix = suffixAt < 0 ? '' : url.slice(suffixAt);
    const target = posix.normalize(posix.join(directory, path));
    assert.ok(
      target.startsWith('docs/'),
      'Documentation link escapes docs: ' + url,
    );

    if (target === 'docs/reference/rust-cli.md')
      return siteBase + '/docs/latest/rust-cli.md' + suffix;
    if (target === 'docs/reference/python-cli.md')
      return siteBase + '/docs/latest/python-cli.md' + suffix;
    const migrated = bySourcePath.get(target);
    if (migrated)
      return siteBase + '/docs/latest/' + migrated.slug + '.md' + suffix;
    return REPO_BLOB + target + suffix;
  });

  const unresolved = [];
  mapMarkdownLinks(published, (url) => {
    if (isRelativeLink(url)) unresolved.push(url);
    return url;
  });
  assert.deepEqual(
    unresolved,
    [],
    document.sourcePath + ' has unresolved export links',
  );
  return published;
}

function validateManifest(manifest) {
  assert.ok(Array.isArray(manifest.documents) && manifest.documents.length > 0);
  const ids = new Set();
  const slugs = new Set();
  for (const document of manifest.documents) {
    assert.match(document.id, /^[a-z0-9]+(?:-[a-z0-9]+)*$/);
    assert.match(document.slug, /^[a-z0-9-]+(?:\/[a-z0-9-]+)*$/);
    assert.equal(document.sourcePath, `${document.slug}.md`);
    for (const field of ['title', 'description', 'group'])
      assert.ok(typeof document[field] === 'string' && document[field].trim());
    assert.ok(!ids.has(document.id), `Duplicate document ID: ${document.id}`);
    assert.ok(
      !slugs.has(document.slug),
      `Duplicate document slug: ${document.slug}`,
    );
    ids.add(document.id);
    slugs.add(document.slug);
  }
  return manifest.documents;
}

export async function refreshSiteDocs({
  projectDir = resolve('.'),
  siteUrl = process.env.NEXT_PUBLIC_SITE_URL || DEFAULT_SITE_URL,
} = {}) {
  const root = resolve(projectDir, '..');
  const manifest = JSON.parse(
    await readFile(join(projectDir, 'lib/data/site-docs.json'), 'utf8'),
  );
  const documents = validateManifest(manifest);
  let repository = true;
  try {
    await access(join(root, 'Cargo.toml'));
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    repository = false;
  }

  // Validate every source before replacing any bundled or public Markdown.
  const copies = await Promise.all(
    documents.map(async (document) => {
      const snapshot = join(projectDir, 'content/docs', document.sourcePath);
      const source = repository
        ? join(root, 'docs', document.sourcePath)
        : snapshot;
      const markdown = await readFile(source, 'utf8');
      const heading = /^# ([^\r\n]+)\r?\n/.exec(markdown)?.[1];
      assert.equal(
        heading?.replace(/`/g, ''),
        document.title,
        document.sourcePath + ' must start with its manifest title',
      );
      return {
        snapshot,
        published: join(
          projectDir,
          'public/docs/latest',
          `${document.slug}.md`,
        ),
        markdown,
        publishedMarkdown: rewritePublishedLinks(
          document,
          markdown,
          documents,
          siteUrl,
        ),
      };
    }),
  );

  for (const { snapshot, published, markdown, publishedMarkdown } of copies) {
    await mkdir(dirname(snapshot), { recursive: true });
    await mkdir(dirname(published), { recursive: true });
    await writeFile(snapshot, markdown);
    await writeFile(published, publishedMarkdown);
  }
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  await refreshSiteDocs();
  console.log('Refreshed site documentation snapshots and Markdown exports.');
}
