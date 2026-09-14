import assert from 'node:assert/strict';
import { cp, copyFile, mkdir, readdir, rm, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export async function preparePages({
  input = resolve('dist/client'),
  output = resolve('dist/pages'),
  basePath = process.env.NEXT_PUBLIC_BASE_PATH || '',
} = {}) {
  assert.ok(
    basePath === '' || /^\/[a-z\d_-]+(?:\/[a-z\d_-]+)*$/i.test(basePath),
    'Invalid Pages base path',
  );
  assert.notEqual(
    resolve(input),
    resolve(output),
    'Pages output must differ from the build directory',
  );
  await rm(output, { recursive: true, force: true });
  await cp(join(input, basePath.slice(1)), output, { recursive: true });
  if (basePath) {
    // Vinext exports the prefixed homepage beside the prefixed asset directory.
    await copyFile(
      join(input, `${basePath.slice(1)}.html`),
      join(output, 'index.html'),
    );
    await copyFile(join(input, '404.html'), join(output, '404.html'));
  }
  const files = await readdir(output, { recursive: true });
  for (const file of files) {
    if (
      !file.endsWith('.html') ||
      file === '404.html' ||
      file.endsWith('index.html')
    )
      continue;
    const destination = join(output, file.slice(0, -5), 'index.html');
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(join(output, file), destination);
  }
  await writeFile(join(output, '.nojekyll'), '');
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  await preparePages();
  console.log('Prepared GitHub Pages artifact in dist/pages.');
}
