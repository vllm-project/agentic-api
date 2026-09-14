import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { createContext, runInContext } from 'node:vm';
import { themeInitScript } from '../lib/theme.mjs';

const source = readFileSync(
  new URL('../lib/theme.mjs', import.meta.url),
  'utf8',
);

function page(saved, unavailable = false) {
  const storage = new Map(saved == null ? [] : [['agentic-api-theme', saved]]);
  const context = createContext({
    window: new EventTarget(),
    Event,
    document: { documentElement: { dataset: {} } },
    localStorage: {
      getItem(key) {
        if (unavailable) throw new Error('Storage unavailable');
        return storage.get(key) ?? null;
      },
      setItem(key, value) {
        if (unavailable) throw new Error('Storage unavailable');
        storage.set(key, value);
      },
      removeItem(key) {
        if (unavailable) throw new Error('Storage unavailable');
        storage.delete(key);
      },
    },
  });
  runInContext(source.replaceAll(/^export /gm, ''), context);
  runInContext(themeInitScript, context);
  return { context, storage };
}

test('initial render restores only valid explicit choices and otherwise follows the system', () => {
  for (const [saved, expected] of [
    [null, 'system'],
    ['invalid', 'system'],
    ['system', 'system'],
    ['light', 'light'],
    ['dark', 'dark'],
  ]) {
    const { context } = page(saved);
    assert.equal(runInContext('readTheme()', context), expected);
  }
});

test('manual changes apply immediately, survive reload, and System removes the override', () => {
  const { context, storage } = page(null);
  for (const choice of ['dark', 'light', 'system']) {
    runInContext(`saveTheme('${choice}')`, context);
    assert.equal(runInContext('readTheme()', context), choice);
    assert.equal(
      storage.get('agentic-api-theme'),
      choice === 'system' ? undefined : choice,
    );
    const reloaded = page(storage.get('agentic-api-theme'));
    assert.equal(runInContext('readTheme()', reloaded.context), choice);
  }
});

test('blocked browser storage preserves system default and in-page theme switching', () => {
  const { context } = page('dark', true);
  assert.equal(runInContext('readTheme()', context), 'system');
  for (const choice of ['light', 'dark', 'system']) {
    runInContext(`saveTheme('${choice}')`, context);
    assert.equal(runInContext('readTheme()', context), choice);
  }
});

test('the theme control is notified after changes and can unsubscribe', () => {
  const { context } = page(null);
  runInContext(
    `
    const observed = [];
    const unsubscribe = subscribeTheme(() => observed.push(readTheme()));
    saveTheme('dark');
    saveTheme('system');
    unsubscribe();
    saveTheme('light');
  `,
    context,
  );
  assert.equal(runInContext('observed.join(",")', context), 'dark,system');
});
