import assert from 'node:assert/strict';
import test from 'node:test';
import { installSpotlights } from '../lib/spotlights.mjs';

function fixture(enabled = true) {
  const media = Object.assign(new EventTarget(), { matches: enabled });
  const frames = new Map();
  let nextFrame = 0;
  const view = Object.assign(new EventTarget(), {
    matchMedia(query) {
      assert(query.includes('(prefers-reduced-motion: no-preference)'));
      assert(query.includes('(pointer: fine)'));
      assert(query.includes('(hover: hover)'));
      return media;
    },
    requestAnimationFrame(callback) {
      frames.set(++nextFrame, callback);
      return nextFrame;
    },
    cancelAnimationFrame(id) {
      frames.delete(id);
    },
  });
  const surfaces = [0, 240].map((left) => {
    const values = new Map();
    return {
      values,
      rect: {
        left,
        top: 0,
        right: left + 200,
        bottom: 200,
        width: 200,
        height: 200,
      },
      style: {
        setProperty: (key, value) => values.set(key, value),
        removeProperty: (key) => values.delete(key),
      },
      getBoundingClientRect() {
        return this.rect;
      },
    };
  });
  const root = { querySelectorAll: () => surfaces };
  const cleanup = installSpotlights({ root, view });
  return {
    cleanup,
    media,
    frames,
    surfaces,
    event(type, fields = {}) {
      view.dispatchEvent(Object.assign(new Event(type), fields));
    },
    pointer(x = 160, y = 80, pointerType = 'mouse') {
      this.event('pointermove', { clientX: x, clientY: y, pointerType });
    },
    flush() {
      const pending = [...frames.values()];
      frames.clear();
      pending.forEach((callback) => callback());
    },
  };
}

test('coalesces pointer moves and illuminates nearby cards before the pointer enters', () => {
  const page = fixture();
  page.pointer(100, 50);
  page.pointer(160, 80);
  assert.equal(page.frames.size, 1);
  assert.equal(page.surfaces[0].values.size, 0);
  page.flush();
  const [active, nearby] = page.surfaces;
  assert.equal(active.values.get('--spotlight-x'), '160.0px');
  assert.equal(active.values.get('--spotlight-y'), '80.0px');
  assert.equal(active.values.get('--spotlight-opacity'), '1.000');
  assert.equal(nearby.values.get('--spotlight-x'), '-80.0px');
  assert(Number(nearby.values.get('--spotlight-opacity')) > 0);
  page.pointer(1000, 1000);
  page.flush();
  assert(
    page.surfaces.every(
      (surface) => surface.values.get('--spotlight-opacity') === '0.000',
    ),
  );
  page.cleanup();
});

test('respects live preference changes and restores the static fallback', () => {
  const page = fixture(false);
  page.pointer();
  assert.equal(page.frames.size, 0);
  page.media.matches = true;
  page.media.dispatchEvent(new Event('change'));
  page.pointer();
  page.flush();
  assert(page.surfaces[0].values.size > 0);
  page.pointer();
  page.media.matches = false;
  page.media.dispatchEvent(new Event('change'));
  assert.equal(page.frames.size, 0);
  assert(page.surfaces.every((surface) => surface.values.size === 0));
  page.pointer();
  assert.equal(page.frames.size, 0);
  page.cleanup();
});

test('repositions after scrolling and clears tracking for touch and window exit', () => {
  const page = fixture();
  page.pointer();
  page.flush();
  page.surfaces[0].rect.left = 100;
  page.event('scroll');
  page.flush();
  assert.equal(page.surfaces[0].values.get('--spotlight-x'), '60.0px');
  page.pointer(160, 80, 'touch');
  assert(page.surfaces.every((surface) => surface.values.size === 0));
  page.pointer();
  page.event('pointerout', { relatedTarget: null });
  assert.equal(page.frames.size, 0);
  assert(page.surfaces.every((surface) => surface.values.size === 0));
  page.cleanup();
});

test('navigation cleanup cancels pending work and removes tracking listeners', () => {
  const page = fixture();
  page.pointer();
  page.flush();
  page.pointer();
  page.cleanup();
  assert.equal(page.frames.size, 0);
  assert(page.surfaces.every((surface) => surface.values.size === 0));
  page.media.dispatchEvent(new Event('change'));
  page.pointer();
  page.event('resize');
  assert.equal(page.frames.size, 0);
});
