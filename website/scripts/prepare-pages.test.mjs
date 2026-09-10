import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { preparePages } from './prepare-pages.mjs';

test('packages a prefixed Vinext export for Pages with working directory routes', async (t) => {
  const root = await mkdtemp(join(tmpdir(), 'pages-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const input = join(root, 'client');
  const output = join(root, 'pages');
  for (const [path, data] of Object.entries({
    'agentic-api.html': '<h1>Home</h1>',
    '404.html': '<h1>Not found</h1>',
    'agentic-api/community/contributors.html': '<h1>Contributors</h1>',
    'agentic-api/community/contributors.txt': 'flight payload',
    'agentic-api/index.txt': 'home payload',
    'agentic-api/_next/static/chunk.js': 'javascript',
    'agentic-api/people/alice.jpg': 'photo',
  })) {
    await mkdir(join(input, path, '..'), { recursive: true });
    await writeFile(join(input, path), data);
  }
  await preparePages({ input, output, basePath: '/agentic-api' });
  for (const [path, data] of Object.entries({
    'index.html': '<h1>Home</h1>',
    '404.html': '<h1>Not found</h1>',
    'community/contributors/index.html': '<h1>Contributors</h1>',
    'community/contributors.txt': 'flight payload',
    'index.txt': 'home payload',
    '_next/static/chunk.js': 'javascript',
    'people/alice.jpg': 'photo',
    '.nojekyll': '',
  }))
    assert.equal(await readFile(join(output, path), 'utf8'), data);
});
