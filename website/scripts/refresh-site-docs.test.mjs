import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { refreshSiteDocs } from './refresh-site-docs.mjs';

const documents = [
  {
    id: 'quickstart',
    slug: 'guides/quickstart',
    sourcePath: 'guides/quickstart.md',
    title: 'Quickstart',
    description: 'Install and send a request.',
    group: 'Start',
  },
  {
    id: 'api',
    slug: 'api/index',
    sourcePath: 'api/index.md',
    title: 'API Reference',
    description: 'Review the API.',
    group: 'API',
  },
];

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'agentic-site-docs-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const projectDir = join(root, 'website');
  await mkdir(join(projectDir, 'lib/data'), { recursive: true });
  await writeFile(
    join(projectDir, 'lib/data/site-docs.json'),
    JSON.stringify({ documents }),
  );
  for (const document of documents) {
    const snapshot = join(projectDir, 'content/docs', document.sourcePath);
    await mkdir(join(projectDir, 'content/docs', document.slug.split('/')[0]), {
      recursive: true,
    });
    await writeFile(snapshot, `# ${document.title}\n\nBundled copy.\n`);
  }
  return { root, projectDir };
}

test('repository docs replace both bundled snapshots and public Markdown exports', async (t) => {
  const { root, projectDir } = await fixture(t);
  await writeFile(join(root, 'Cargo.toml'), '[workspace]\n');
  for (const document of documents) {
    const source = join(root, 'docs', document.sourcePath);
    await mkdir(join(root, 'docs', document.slug.split('/')[0]), {
      recursive: true,
    });
    await writeFile(source, `# ${document.title}\n\nCurrent source.\n`);
  }

  await refreshSiteDocs({ projectDir });
  for (const document of documents) {
    for (const path of [
      join(projectDir, 'content/docs', document.sourcePath),
      join(projectDir, 'public/docs/latest', `${document.slug}.md`),
    ])
      assert.equal(
        await readFile(path, 'utf8'),
        `# ${document.title}\n\nCurrent source.\n`,
      );
  }
});

test('standalone site exports committed snapshots without repository docs', async (t) => {
  const { projectDir } = await fixture(t);
  await refreshSiteDocs({ projectDir });
  for (const document of documents)
    assert.equal(
      await readFile(
        join(projectDir, 'public/docs/latest', `${document.slug}.md`),
        'utf8',
      ),
      `# ${document.title}\n\nBundled copy.\n`,
    );
});

test('a broken repository source fails without publishing stale snapshots', async (t) => {
  const { root, projectDir } = await fixture(t);
  await writeFile(join(root, 'Cargo.toml'), '[workspace]\n');
  const source = join(root, 'docs/guides/quickstart.md');
  await mkdir(join(root, 'docs/guides'), { recursive: true });
  await mkdir(join(root, 'docs/api'), { recursive: true });
  await writeFile(source, '# Wrong title\n');
  await writeFile(join(root, 'docs/api/index.md'), '# API Reference\n');

  await assert.rejects(
    refreshSiteDocs({ projectDir }),
    /guides\/quickstart\.md must start with its manifest title/,
  );
  assert.equal(
    await readFile(
      join(projectDir, 'content/docs/guides/quickstart.md'),
      'utf8',
    ),
    '# Quickstart\n\nBundled copy.\n',
  );
  await assert.rejects(
    readFile(join(projectDir, 'public/docs/latest/guides/quickstart.md')),
    { code: 'ENOENT' },
  );
});

test('published links point to raw site docs, CLI references, or GitHub source', async (t) => {
  const { root, projectDir } = await fixture(t);
  await writeFile(join(root, 'Cargo.toml'), '[workspace]\n');
  await mkdir(join(root, 'docs/guides'), { recursive: true });
  await mkdir(join(root, 'docs/api'), { recursive: true });
  const markdown = [
    '# Quickstart',
    '',
    '[Responses](../api/index.md#responses)',
    '[CLI](../reference/rust-cli.md#run)',
    '[Deployment](../deploying/kubernetes.md#configure)',
    '[External](https://example.com/guide)',
    '[On this page](#next)',
    '[More]: ../developing/releases.md#publish',
    '',
    '```text',
    '[Example only](../api/index.md)',
    '```',
    '',
  ].join('\n');
  await writeFile(join(root, 'docs/guides/quickstart.md'), markdown);
  await writeFile(join(root, 'docs/api/index.md'), '# API Reference\n');

  await refreshSiteDocs({
    projectDir,
    siteUrl: 'https://docs.example.test/agentic-api',
  });
  assert.equal(
    await readFile(
      join(projectDir, 'content/docs/guides/quickstart.md'),
      'utf8',
    ),
    markdown,
  );
  const exported = await readFile(
    join(projectDir, 'public/docs/latest/guides/quickstart.md'),
    'utf8',
  );
  assert.match(
    exported,
    /\[Responses\]\(https:\/\/docs\.example\.test\/agentic-api\/docs\/latest\/api\/index\.md#responses\)/,
  );
  assert.match(
    exported,
    /\[CLI\]\(https:\/\/docs\.example\.test\/agentic-api\/docs\/latest\/rust-cli\.md#run\)/,
  );
  assert.match(
    exported,
    /\[Deployment\]\(https:\/\/github\.com\/vllm-project\/agentic-api\/blob\/main\/docs\/deploying\/kubernetes\.md#configure\)/,
  );
  assert.match(
    exported,
    /\[More\]: https:\/\/github\.com\/vllm-project\/agentic-api\/blob\/main\/docs\/developing\/releases\.md#publish/,
  );
  assert.match(exported, /\[External\]\(https:\/\/example\.com\/guide\)/);
  assert.match(exported, /\[On this page\]\(#next\)/);
  assert.match(
    exported,
    /```text\n\[Example only\]\(\.\.\/api\/index\.md\)\n```/,
  );
  const outsideFence = exported.replace(/```text\n[\s\S]*?\n```/g, '');
  assert.doesNotMatch(outsideFence, /\]\(\.\.?\//);
  assert.doesNotMatch(outsideFence, /^\[[^\]]+\]:\s*\.\.?\//m);
});

test('source title validation accepts inline code while display title stays plain', async (t) => {
  const { projectDir } = await fixture(t);
  await writeFile(
    join(projectDir, 'content/docs/guides/quickstart.md'),
    '# `Quickstart`\n\nBundled copy.\n',
  );
  await refreshSiteDocs({ projectDir });
  assert.equal(
    await readFile(
      join(projectDir, 'public/docs/latest/guides/quickstart.md'),
      'utf8',
    ),
    '# `Quickstart`\n\nBundled copy.\n',
  );
});
