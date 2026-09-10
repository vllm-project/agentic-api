import assert from 'node:assert/strict';
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const api = 'https://api.github.com';
const endpoint = '/repos/vllm-project/agentic-api/contributors';

export async function refreshContributors({
  root = fileURLToPath(new URL('../', import.meta.url)),
  fetchImpl = fetch,
  token = process.env.GH_TOKEN || process.env.GITHUB_TOKEN,
  now = new Date(),
} = {}) {
  const filename = resolve(root, 'lib/data/community.json');
  const snapshot = JSON.parse(await readFile(filename, 'utf8'));
  const headers = {
    Accept: 'application/vnd.github+json',
    'X-GitHub-Api-Version': '2026-03-10',
    ...(token ? { Authorization: `Bearer ${token}` } : {}),
  };
  async function request(url, isApi = true) {
    const response = await fetchImpl(url, {
      headers: isApi ? headers : {},
      redirect: 'error',
      signal: AbortSignal.timeout(15_000),
    });
    if (!response.ok)
      throw new Error(
        `GitHub request failed (${response.status}) for ${new URL(url).pathname}`,
      );
    return response;
  }

  const contributors = [];
  const visited = new Set();
  let next = `${api}${endpoint}?per_page=100`;
  while (next) {
    const url = new URL(next);
    assert.ok(
      url.origin === api && url.pathname === endpoint,
      'Invalid contributor pagination URL',
    );
    assert.ok(
      !visited.has(url.href) && visited.size < 100,
      'Contributor pagination did not finish',
    );
    visited.add(url.href);
    const response = await request(url);
    const page = await response.json();
    assert.ok(Array.isArray(page), 'Invalid contributor response');
    contributors.push(...page.filter((person) => person.type === 'User'));
    next = response.headers.get('link')?.match(/<([^>]+)>;\s*rel="next"/)?.[1];
  }
  assert.ok(
    contributors.length > 0,
    'Refusing to replace contributors with an empty list',
  );
  const logins = new Set();
  for (const person of contributors) {
    assert.match(
      person.login,
      /^[a-z\d](?:[a-z\d-]{0,37}[a-z\d])?$/i,
      'Invalid contributor login',
    );
    assert.ok(
      !logins.has(person.login.toLowerCase()),
      'Duplicate contributor login',
    );
    logins.add(person.login.toLowerCase());
    assert.ok(
      Number.isSafeInteger(person.contributions) && person.contributions >= 0,
      'Invalid contribution count',
    );
  }
  contributors.sort((a, b) => b.contributions - a.contributions);

  // Fetch everything before replacing the snapshot or any bundled photos.
  const refreshed = [];
  for (let index = 0; index < contributors.length; index += 4) {
    const batch = await Promise.all(
      contributors.slice(index, index + 4).map(async (person) => {
        const profile = await (
          await request(`${api}/users/${person.login}`)
        ).json();
        const avatarUrl = new URL(person.avatar_url);
        assert.equal(
          avatarUrl.origin,
          'https://avatars.githubusercontent.com',
          'Invalid avatar host',
        );
        avatarUrl.searchParams.set('s', '160');
        const response = await request(avatarUrl, false);
        const type = response.headers.get('content-type')?.split(';')[0];
        assert.ok(
          type === 'image/jpeg' || type === 'image/png',
          'Invalid avatar content type',
        );
        const bytes = Buffer.from(await response.arrayBuffer());
        assert.ok(
          bytes.length > 0 && bytes.length <= 2 * 1024 * 1024,
          'Invalid avatar size',
        );
        return {
          person: {
            login: person.login,
            name:
              (typeof profile.name === 'string' && profile.name.trim()) ||
              person.login,
            url: `https://github.com/${person.login}`,
            avatar: `/people/${person.login}.${type === 'image/png' ? 'png' : 'jpg'}`,
          },
          bytes,
        };
      }),
    );
    refreshed.push(...batch);
  }
  await mkdir(resolve(root, 'public/people'), { recursive: true });
  for (const { person, bytes } of refreshed) {
    await writeFile(resolve(root, `public${person.avatar}`), bytes);
  }
  const updated = {
    ...snapshot,
    updatedAt: now.toISOString().slice(0, 10),
    source: `${api}${endpoint}?per_page=100`,
    contributors: refreshed.map(({ person }) => person),
  };
  await writeFile(`${filename}.tmp`, JSON.stringify(updated, null, 2) + '\n');
  await rename(`${filename}.tmp`, filename);
  return updated.contributors.length;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  try {
    console.log(
      `Refreshed ${await refreshContributors()} contributor profiles and photos.`,
    );
  } catch (error) {
    console.error(
      `Contributor refresh failed; deployment stopped: ${error.message}`,
    );
    process.exitCode = 1;
  }
}
