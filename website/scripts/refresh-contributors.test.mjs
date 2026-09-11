import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { refreshContributors } from './refresh-contributors.mjs';

const api = 'https://api.github.com';
const source = `${api}/repos/vllm-project/agentic-api/contributors?per_page=100`;
const person = (login, contributions) => ({
  login,
  contributions,
  type: 'User',
  html_url: `https://github.com/${login}`,
  avatar_url: `https://avatars.githubusercontent.com/u/${login}`,
});
const json = (data, headers = {}) => Response.json(data, { headers });
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'contributors-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, 'lib/data'), { recursive: true });
  await mkdir(join(root, 'public/people'), { recursive: true });
  const snapshot = {
    updatedAt: '2026-09-09',
    source,
    maintainers: [{ login: 'owner' }],
    contributors: [
      {
        login: 'old',
        name: 'Old',
        avatar: '/people/old.jpg',
        url: 'https://github.com/old',
      },
    ],
  };
  await writeFile(
    join(root, 'lib/data/community.json'),
    JSON.stringify(snapshot),
  );
  await writeFile(join(root, 'public/people/old.jpg'), 'existing photo');
  return { root, snapshot };
}

test('refreshes every page and profile, preserving tie order without exposing counts', async (t) => {
  const { root, snapshot } = await fixture(t);
  const fetchImpl = async (input, options) => {
    const url = new URL(input);
    if (url.origin === api)
      assert.equal(options.headers.Authorization, 'Bearer test-token');
    else assert.equal(options.headers?.Authorization, undefined);
    if (url.pathname.endsWith('/contributors')) {
      if (url.searchParams.get('page') === '2')
        return json([person('alice', 9), person('charlie', 2)]);
      return json([person('bob', 2)], {
        link: `<${source}&page=2>; rel="next"`,
      });
    }
    if (url.pathname.startsWith('/users/'))
      return json({
        name: url.pathname.endsWith('alice') ? ' Alice Updated ' : null,
      });
    return new Response('photo bytes', {
      headers: { 'content-type': 'image/jpeg' },
    });
  };
  await refreshContributors({
    root,
    fetchImpl,
    token: 'test-token',
    now: new Date('2026-09-10T01:00:00Z'),
  });
  const actual = JSON.parse(
    await readFile(join(root, 'lib/data/community.json')),
  );
  assert.equal(actual.updatedAt, '2026-09-10');
  assert.deepEqual(actual.maintainers, snapshot.maintainers);
  assert.deepEqual(actual.contributors, [
    {
      login: 'alice',
      name: 'Alice Updated',
      url: 'https://github.com/alice',
      avatar: '/people/alice.jpg',
    },
    {
      login: 'bob',
      name: 'bob',
      url: 'https://github.com/bob',
      avatar: '/people/bob.jpg',
    },
    {
      login: 'charlie',
      name: 'charlie',
      url: 'https://github.com/charlie',
      avatar: '/people/charlie.jpg',
    },
  ]);
  assert.equal(
    await readFile(join(root, 'public/people/alice.jpg'), 'utf8'),
    'photo bytes',
  );
});

for (const failure of [
  'later page',
  'profile',
  'avatar',
  'empty list',
  'invalid login',
  'foreign pagination',
]) {
  test(`${failure} failure leaves the saved roster and photos intact`, async (t) => {
    const { root, snapshot } = await fixture(t);
    const before = await readFile(
      join(root, 'lib/data/community.json'),
      'utf8',
    );
    const fetchImpl = async (input) => {
      const url = new URL(input);
      assert.equal(
        ['api.github.com', 'avatars.githubusercontent.com'].includes(
          url.hostname,
        ),
        true,
      );
      if (url.pathname.endsWith('/contributors')) {
        if (failure === 'empty list') return json([]);
        if (failure === 'invalid login') return json([person('../escape', 1)]);
        if (failure === 'foreign pagination')
          return json([person('old', 1)], {
            link: '<https://example.com/steal>; rel="next"',
          });
        if (failure === 'later page')
          return url.searchParams.has('page')
            ? new Response('', { status: 503 })
            : json([person('old', 1)], {
                link: `<${source}&page=2>; rel="next"`,
              });
        return json([person('old', 1)]);
      }
      if (url.pathname.startsWith('/users/'))
        return failure === 'profile'
          ? new Response('', { status: 403 })
          : json({ name: 'Changed' });
      return failure === 'avatar'
        ? new Response('', { status: 503 })
        : new Response('new photo', {
            headers: { 'content-type': 'image/jpeg' },
          });
    };
    const expectedError = {
      'later page': /GitHub request failed \(503\)/,
      profile: /GitHub request failed \(403\)/,
      avatar: /GitHub request failed \(503\)/,
      'empty list': /empty list/,
      'invalid login': /Invalid contributor login/,
      'foreign pagination': /Invalid contributor pagination URL/,
    }[failure];
    await assert.rejects(
      refreshContributors({ root, fetchImpl }),
      expectedError,
    );
    assert.equal(
      await readFile(join(root, 'lib/data/community.json'), 'utf8'),
      before,
    );
    assert.equal(
      await readFile(join(root, 'public/people/old.jpg'), 'utf8'),
      'existing photo',
    );
    assert.equal(snapshot.updatedAt, '2026-09-09');
  });
}
