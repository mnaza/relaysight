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
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const web = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = name => readFileSync(join(web, name), 'utf8');

/** Selectors the script hands to querySelector/getElementById on `document`. */
function selectorsIn(source) {
  const found = new Set();
  for (const m of source.matchAll(/document\.querySelector(?:All)?\(\s*'([^']+)'/g)) found.add(m[1]);
  for (const m of source.matchAll(/document\.getElementById\(\s*['"]([^'"]+)['"]/g)) found.add('#' + m[1]);
  return [...found];
}

for (const [script, page] of [['dashboard.js', 'app.html'], ['landing.js', 'index.html']]) {
  test(`every element ${script} looks for exists in ${page}`, () => {
    const document = new JSDOM(read(page)).window.document;
    const selectors = selectorsIn(read(script));
    assert.ok(selectors.length > 0, `no selectors found in ${script}; the scan is broken`);

    const missing = selectors.filter(selector => {
      // Attribute selectors match nodes the script creates or annotates at
      // runtime, so their absence from the static page is not a fault.
      if (selector.startsWith('[')) return false;
      return document.querySelector(selector) === null;
    });
    assert.deepEqual(missing, [], `${page} has no element for these`);
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
  for (const page of ['index.html', 'app.html']) {
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
  for (const page of ['index.html', 'app.html']) {
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
  for (const page of ['index.html', 'app.html']) {
    const source = read(page);
    assert.doesNotMatch(source, /localhost:\d+/, `${page} points at localhost`);
    assert.doesNotMatch(source, /\b(?:127\.0\.0\.1|192\.168\.\d+\.\d+)\b/, `${page} points at a private address`);
  }
});
