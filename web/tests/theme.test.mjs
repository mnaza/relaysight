// The pure half of theme.js. Everything else touches `document`, so it belongs
// to a browser test rather than this file.
import test from 'node:test';
import assert from 'node:assert/strict';
import { t, mergeBrand } from '../theme.js';

test('t returns the translation for a known key', () => {
  assert.equal(t({ 'a.b': 'Hello' }, 'a.b'), 'Hello');
});

test('t falls back to the key itself, which is why missing keys are silent', () => {
  // Documenting the behaviour the i18n tests exist to compensate for: this
  // never throws, so a missing key ships to production looking like a label.
  assert.equal(t({}, 'app.live.connecting'), 'app.live.connecting');
  assert.equal(t({}, 'app.live.connecting', 'Connecting…'), 'Connecting…');
});

test('t substitutes variables', () => {
  assert.equal(t({ k: '{n} cameras' }, 'k', 'k', { n: 3 }), '3 cameras');
  assert.equal(t({ k: '{a} of {b}' }, 'k', 'k', { a: 1, b: 2 }), '1 of 2');
});

test('t leaves an unsupplied placeholder alone rather than printing undefined', () => {
  // "{count} cameras" is a visible bug; "undefined cameras" is a worse one.
  assert.equal(t({ k: '{count} cameras' }, 'k'), '{count} cameras');
});

test('t coerces non-string values instead of throwing', () => {
  assert.equal(t({ k: 7 }, 'k'), '7');
});

test('mergeBrand overrides scalars and merges the nested groups', () => {
  const base = {
    name: 'RelaySight',
    defaultLocale: 'en',
    theme: { accent: '#111', bg: '#fff' },
    freeTier: { cameras: 3 },
    gateway: { url: 'https://a' },
    contact: { email: 'a@example.com' },
  };
  const merged = mergeBrand(base, { name: 'Acme', theme: { accent: '#f00' } });

  assert.equal(merged.name, 'Acme');
  assert.equal(merged.defaultLocale, 'en', 'untouched scalars must survive');
  assert.equal(merged.theme.accent, '#f00');
  assert.equal(merged.theme.bg, '#fff', 'a partial theme override must not wipe the rest');
  assert.deepEqual(merged.freeTier, { cameras: 3 });
});

test('mergeBrand tolerates a base with none of the nested groups', () => {
  // White-label overrides arrive from localStorage and may be anything.
  const merged = mergeBrand({ name: 'Base' }, { theme: { accent: '#0f0' } });
  assert.equal(merged.theme.accent, '#0f0');
  assert.deepEqual(merged.freeTier, {});
  assert.deepEqual(merged.contact, {});
});
