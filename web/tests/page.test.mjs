// The contract between the scripts and the markup.
//
// dashboard.js reaches for 38 elements by selector and does nothing to check it
// found them. Rename an id in app.html and the first `.appendChild` on null
// throws during module evaluation, which stops the whole script: the page loads
// and stays empty, with one line in a console nobody has open. That failure is
// invisible to every other test in this repository, and it is a rename away.
import test from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { pages, scriptsFor, web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');

/** Selectors the script hands to querySelector/getElementById on `document`. */
function selectorsIn(source) {
  const found = new Set();
  for (const m of source.matchAll(/document\.querySelector(?:All)?\(\s*'([^']+)'/g)) found.add(m[1]);
  for (const m of source.matchAll(/document\.getElementById\(\s*['"]([^'"]+)['"]/g)) found.add('#' + m[1]);
  return [...found];
}

for (const page of pages) {
  test(`every element the scripts of ${page} look for exists in it`, () => {
    const document = new JSDOM(read(page)).window.document;
    // Every script the page loads, including modules it imports — a selector
    // does not stop mattering because it moved into another file.
    const selectors = scriptsFor(page).flatMap(script => selectorsIn(read(script)));

    // Only id selectors. Those are the anchors the scripts append into without
    // checking, so a missing one throws and stops the module. Class and
    // attribute lookups are the optional kind — `applyBrand` looks for a
    // favicon link precisely so it can create one when there is none — and
    // demanding those exist would fail on correct code.
    const required = selectors.filter(selector => /^#[\w-]+$/.test(selector));
    assert.ok(required.length > 0, `no id selectors found for ${page}; the scan is broken`);
    const missing = required.filter(selector => document.querySelector(selector) === null);
    assert.deepEqual([...new Set(missing)], [], `${page} has no element for these`);
  });
}

test('the app shell has the anchors the bootstrap appends into', () => {
  // These four run unguarded at module scope, before any error handling. If one
  // is absent nothing on the page renders at all.
  const document = new JSDOM(read('app.html')).window.document;
  for (const id of ['#app-brand', '#app-locale']) {
    assert.notEqual(document.querySelector(id), null, `${id} is where the bootstrap appends`);
  }
  assert.ok(
    document.querySelectorAll('[data-i18n]').length > 0,
    'nothing on the page is marked for translation',
  );
});

test('both pages declare a language and a viewport', () => {
  for (const page of pages) {
    const document = new JSDOM(read(page)).window.document;
    assert.ok(document.documentElement.getAttribute('lang'), `${page} has no lang attribute`);
    assert.notEqual(
      document.querySelector('meta[name="viewport"]'),
      null,
      `${page} has no viewport meta, so it renders zoomed out on a phone`,
    );
  }
});

test('every script and stylesheet a page references is a file that exists', () => {
  // A path typo gives a page that loads and does nothing, with a 404 only in
  // the network tab.
  for (const page of pages) {
    const document = new JSDOM(read(page)).window.document;
    const refs = [
      ...[...document.querySelectorAll('script[src]')].map(n => n.getAttribute('src')),
      ...[...document.querySelectorAll('link[rel="stylesheet"]')].map(n => n.getAttribute('href')),
    ].filter(href => href && !href.startsWith('http') && !href.startsWith('//'));
    assert.ok(refs.length > 0, `${page} references no local assets`);
    for (const ref of refs) {
      assert.ok(existsSync(join(web, ref.replace(/^\.?\//, ''))), `${page} references missing ${ref}`);
    }
  }
});

test('no page ships a hardcoded localhost or private address', () => {
  // Easy to leave behind while developing and it breaks every deployment but
  // the author's.
  for (const page of pages) {
    const source = read(page);
    assert.doesNotMatch(source, /localhost:\d+/, `${page} points at localhost`);
    assert.doesNotMatch(source, /\b(?:127\.0\.0\.1|192\.168\.\d+\.\d+)\b/, `${page} points at a private address`);
  }
});
