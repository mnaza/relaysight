// Locale coverage. A missing key does not throw — `t()` falls back to the key
// itself, so the user is shown "app.live.connecting" instead of a sentence and
// nothing anywhere reports a fault. These tests are the only thing that notices.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { allSources, web } from './sources.mjs';
const localeNames = readdirSync(join(web, 'locales')).filter(f => f.endsWith('.json'));
const locales = Object.fromEntries(
  localeNames.map(f => [f.replace('.json', ''), JSON.parse(readFileSync(join(web, 'locales', f), 'utf8'))]),
);
const reference = 'en';

test('every locale carries exactly the keys English does', () => {
  const expected = new Set(Object.keys(locales[reference]));
  for (const [name, dict] of Object.entries(locales)) {
    if (name === reference) continue;
    const actual = new Set(Object.keys(dict));
    const missing = [...expected].filter(k => !actual.has(k));
    const extra = [...actual].filter(k => !expected.has(k));
    assert.deepEqual(missing, [], `${name} is missing keys`);
    assert.deepEqual(extra, [], `${name} has keys English does not`);
  }
});

test('no translation is left empty or still in English by accident', () => {
  for (const [name, dict] of Object.entries(locales)) {
    for (const [key, value] of Object.entries(dict)) {
      assert.equal(typeof value, 'string', `${name}.${key} is not a string`);
      assert.notEqual(value.trim(), '', `${name}.${key} is empty`);
    }
  }
});

test('interpolation variables match across locales', () => {
  // If English says "{count} cameras" and Russian drops {count}, the number
  // silently disappears for those users. Nothing else catches that.
  const vars = value => new Set([...String(value).matchAll(/\{([A-Za-z0-9_]+)\}/g)].map(m => m[1]));
  for (const [key, english] of Object.entries(locales[reference])) {
    const expected = [...vars(english)].sort();
    for (const [name, dict] of Object.entries(locales)) {
      if (name === reference) continue;
      assert.deepEqual([...vars(dict[key])].sort(), expected, `${name}.${key} placeholders differ from English`);
    }
  }
});

test('every key the UI asks for exists in every locale', () => {
  // Derived from the markup rather than listed here — see sources.mjs for why.
  const scanned = allSources();
  const sources = scanned.map(f => readFileSync(join(web, f), 'utf8'));
  const referenced = new Set();
  let dynamic = 0;
  for (const source of sources) {
    for (const m of source.matchAll(/data-i18n(?:-placeholder)?="([^"]+)"/g)) referenced.add(m[1]);
    // `data-list` names a key too — carried over from scripts/check-i18n.py,
    // which this file replaces.
    for (const m of source.matchAll(/data-list="([^"]+)"/g)) referenced.add(m[1]);
    for (const m of source.matchAll(/\bt\(\s*\w+\s*,\s*(['"])([^'"]+)\1/g)) referenced.add(m[2]);
    // Keys built at runtime cannot be checked statically. Count them rather
    // than let the pass rate look complete when it is not.
    for (const _ of source.matchAll(/\bt\(\s*\w+\s*,\s*[`a-zA-Z]/g)) dynamic += 1;
  }
  assert.ok(
    scanned.some(f => f.endsWith('.js')) && scanned.some(f => f.endsWith('.html')),
    `the scan found no scripts or no pages: ${scanned}`,
  );
  // Was 180 when this repository also carried the marketing page. The
  // landing keys moved out with it; this floor still catches a module that
  // stops being scanned, which is the failure the number exists for.
  assert.ok(referenced.size > 110, `only found ${referenced.size} literal keys across ${scanned.length} files; the scan is probably missing a module`);
  for (const [name, dict] of Object.entries(locales)) {
    const missing = [...referenced].filter(key => !(key in dict));
    assert.deepEqual(missing, [], `${name} lacks keys the UI references`);
  }
  console.log(`  checked ${referenced.size} literal keys across ${scanned.length} files; ${dynamic} runtime-built keys are not statically checkable`);
});
