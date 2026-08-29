// Rendering, in a real DOM. These functions run on every page load — the header
// mark, the language picker and the palette — and none of them was reachable
// from a test until jsdom was added.
import test, { before } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const web = join(dirname(fileURLToPath(import.meta.url)), '..');
const brand = JSON.parse(readFileSync(join(web, 'brand.json'), 'utf8'));

let applyBrand, brandMark, localeSelect;

function freshDom() {
  const dom = new JSDOM('<!doctype html><html><head></head><body></body></html>', {
    url: 'https://example.test/',
  });
  globalThis.document = dom.window.document;
  globalThis.window = dom.window;
  globalThis.location = dom.window.location;
  globalThis.localStorage = dom.window.localStorage;
  globalThis.URL = dom.window.URL;
  return dom;
}

before(async () => {
  // theme.js is imported after the globals exist, because its exports build
  // elements through `document` as soon as they are called.
  freshDom();
  ({ applyBrand, brandMark, localeSelect } = await import('../theme.js'));
});

test('applyBrand writes the palette into CSS custom properties', () => {
  freshDom();
  applyBrand({ name: 'Acme', theme: { primary: '#123456', accent: '#abcdef', radius: 4 } });
  const root = document.documentElement;
  assert.equal(root.style.getPropertyValue('--primary'), '#123456');
  assert.equal(root.style.getPropertyValue('--accent'), '#abcdef');
  assert.equal(root.style.getPropertyValue('--radius'), '4px', 'radius needs its unit');
});

test('applyBrand defaults the radius rather than emitting NaNpx', () => {
  freshDom();
  applyBrand({ name: 'Acme', theme: {} });
  assert.equal(document.documentElement.style.getPropertyValue('--radius'), '18px');
});

test('applyBrand leaves a property alone when the brand omits it', () => {
  // Writing an empty value would override the stylesheet default with nothing.
  freshDom();
  applyBrand({ name: 'Acme', theme: { primary: '#123456' } });
  assert.equal(document.documentElement.style.getPropertyValue('--surface'), '');
});

test('applyBrand sets the document title', () => {
  freshDom();
  applyBrand({ name: 'Acme Video' });
  assert.equal(document.title, 'Acme Video');
});

test('applyBrand adds a favicon link only when one is configured', () => {
  freshDom();
  applyBrand({ name: 'Acme' });
  assert.equal(document.querySelector('link[rel="icon"]'), null);

  freshDom();
  applyBrand({ name: 'Acme', faviconUrl: '/logo.ico' });
  assert.equal(document.querySelector('link[rel="icon"]').getAttribute('href'), '/logo.ico');
});

test('applyBrand does not append the custom stylesheet twice', () => {
  // It runs on every navigation within the app shell; duplicates would stack.
  freshDom();
  applyBrand({ name: 'Acme', customCssUrl: '/skin.css' });
  applyBrand({ name: 'Acme', customCssUrl: '/skin.css' });
  assert.equal(document.querySelectorAll('[data-brand-css]').length, 1);
});

test('brandMark shows the name and links home', () => {
  freshDom();
  const mark = brandMark({ name: 'Acme' });
  assert.equal(mark.tagName, 'A');
  assert.equal(mark.getAttribute('href'), '/');
  assert.match(mark.textContent, /Acme/);
});

test('brandMark falls back to an initial when there is no logo', () => {
  freshDom();
  const mark = brandMark({ name: 'acme' });
  assert.equal(mark.querySelector('.brand-mark').textContent, 'A');
  assert.equal(mark.querySelector('img'), null);
});

test('brandMark uses the logo when one is given, with the name as alt text', () => {
  freshDom();
  const mark = brandMark({ name: 'Acme', logoUrl: '/logo.svg' });
  const image = mark.querySelector('img');
  assert.equal(image.getAttribute('src'), '/logo.svg');
  assert.equal(image.getAttribute('alt'), 'Acme', 'the logo needs alt text');
});

test('the compact mark drops the wordmark but keeps the link', () => {
  freshDom();
  const mark = brandMark({ name: 'Acme' }, true);
  assert.equal(mark.querySelector('strong'), null);
  assert.equal(mark.getAttribute('href'), '/');
});

test('localeSelect lists exactly the supported locales and preselects the current one', () => {
  freshDom();
  const select = localeSelect({ supportedLocales: ['en', 'es', 'ru'] }, 'ru');
  assert.deepEqual([...select.options].map(o => o.value), ['en', 'es', 'ru']);
  assert.equal(select.value, 'ru');
  assert.equal(select.getAttribute('aria-label'), 'Language', 'the picker needs a label');
});

test('localeSelect renders nothing selectable when no locales are configured', () => {
  freshDom();
  const select = localeSelect({}, 'en');
  assert.equal(select.options.length, 0);
});

test('choosing a language stores it and reloads with the parameter set', () => {
  const dom = freshDom();
  const select = localeSelect({ supportedLocales: ['en', 'ru'] }, 'en');
  dom.window.document.body.appendChild(select);
  select.value = 'ru';
  select.dispatchEvent(new dom.window.Event('change'));
  assert.equal(dom.window.localStorage.getItem('locale'), 'ru');
});

test('the shipped brand.json renders without special-casing', () => {
  // The defaults are what a fresh deployment shows, so they have to work.
  freshDom();
  applyBrand(brand);
  assert.equal(document.title, brand.name);
  const select = localeSelect(brand, brand.defaultLocale);
  assert.equal(select.value, brand.defaultLocale);
});
