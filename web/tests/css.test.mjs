// The stylesheet, as a contract rather than as layout.
//
// jsdom has no layout engine, so nothing here can tell you a grid collapsed or
// an element ended up off screen — that needs a real browser. What it can check
// is the contract between the theme and the rules, which is where white-labelling
// fails silently: the value lands on the element, no rule reads it, and the
// customer's colour simply does nothing.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const css = readFileSync(join(web, 'styles.css'), 'utf8');
const theme = readFileSync(join(web, 'theme.js'), 'utf8');
const brand = JSON.parse(readFileSync(join(web, 'brand.json'), 'utf8'));

const defined = new Set([...css.matchAll(/(--[\w-]+)\s*:/g)].map(m => m[1]));
const read = new Set([...css.matchAll(/var\(\s*(--[\w-]+)/g)].map(m => m[1]));
/** Custom properties `applyBrand` writes onto the root element. */
const written = new Set([...theme.matchAll(/'(--[\w-]+)':/g)].map(m => m[1]));

test('every var() names a property the stylesheet defines', () => {
  // An undefined custom property resolves to nothing, so the declaration is
  // dropped and the element keeps whatever it inherited. No error anywhere.
  const undefinedVars = [...read].filter(name => !defined.has(name)).sort();
  assert.deepEqual(undefinedVars, []);
});

test('every property the brand can set has a default in the stylesheet', () => {
  // The default is what an unbranded deployment shows, and what remains if a
  // brand omits one — applyBrand skips empty values rather than writing them.
  const missing = [...written].filter(name => !defined.has(name)).sort();
  assert.deepEqual(missing, []);
});

test('every property a brand can set is read by at least one rule', () => {
  // The failure this guards against has no symptom: the value lands on the root
  // element, no rule reads it, and the customer's colour does nothing. `--surface`
  // was in exactly that state — written by applyBrand, offered as a colour picker
  // in the branding modal, consumed nowhere.
  const orphans = [...written].filter(name => !read.has(name)).sort();
  assert.deepEqual(orphans, []);
});

test('no custom property is defined and then never read', () => {
  // The other end of the same problem. `--surface-2` and `--glass` sat here as
  // leftovers of a palette that moved on, which is how a stylesheet accumulates
  // values nobody can tell are dead.
  const unread = [...defined].filter(name => !read.has(name)).sort();
  assert.deepEqual(unread, []);
});

test('the app chrome that is still hardcoded is exactly this list', () => {
  // The panels and cards now read --surface, but the rest of the shell still
  // paints literal hex: the body, the sidebar, the modal, the players and the
  // inputs. Each is a different shade of the same ladder, and deriving them from
  // --surface would change how the default theme looks, so they wait for that
  // decision. Pinned as an explicit set so a new one cannot appear without
  // someone choosing to add it.
  const body = css.slice(css.indexOf('}') + 1);
  const hardcoded = [...body.matchAll(/([.#][\w-]+)[^{}]*\{[^{}]*background:\s*(#[0-9a-fA-F]{3,8})/g)]
    .map(m => m[1])
    .sort();
  assert.deepEqual(hardcoded, [
    '.ai-frame',
    '.app-body',
    '.archive-player',
    '.codebox',
    '.field',
    '.field',
    '.live-player',
    '.modal',
    '.sidebar',
    '.telemetry-metric',
    '.timeline-row',
  ]);
});

test('the shipped brand agrees with the stylesheet defaults', () => {
  // Two ways to render an unbranded deployment — no brand at all, or the brand
  // that ships — and they must look the same. If they drift, the page changes
  // appearance the moment brand.json loads, which reads as a flash of the wrong
  // theme and is hard to attribute to anything.
  const root = css.slice(css.indexOf(':root'), css.indexOf('}'));
  const defaults = Object.fromEntries(
    [...root.matchAll(/(--[\w-]+):\s*([^;]+);/g)].map(m => [m[1], m[2].trim()]),
  );
  for (const [name, value] of Object.entries(brand.theme ?? {})) {
    if (name === 'mode') continue;
    const declared = defaults['--' + name];
    if (declared === undefined) continue;
    const expected = name === 'radius' ? `${value}px` : String(value);
    assert.equal(declared, expected, `brand.json ${name} differs from the --${name} default`);
  }
});

test('the stylesheet has balanced braces', () => {
  // A stray brace silently swallows every rule after it, and the page renders
  // half-styled with nothing in the console.
  let depth = 0;
  for (const char of css) {
    if (char === '{') depth += 1;
    else if (char === '}') depth -= 1;
    assert.ok(depth >= 0, 'a closing brace with nothing open');
  }
  assert.equal(depth, 0, 'unclosed rule');
});

test('the stylesheet pulls in nothing from another host', () => {
  // A remote @import is a render-blocking third-party request on a page that is
  // otherwise entirely self-hosted, and it breaks on an isolated network —
  // which is where cameras live.
  assert.doesNotMatch(css, /@import\s+url\(\s*['"]?https?:/i);
  assert.doesNotMatch(css, /url\(\s*['"]?https?:\/\//i, 'a remote asset is referenced');
});

test('the layout adapts to a phone', () => {
  const queries = [...css.matchAll(/@media[^{]+/g)].map(m => m[0]);
  assert.ok(queries.length > 0, 'no media queries at all');
  const widths = queries
    .flatMap(q => [...q.matchAll(/max-width:\s*(\d+)px/g)].map(m => Number(m[1])));
  assert.ok(
    widths.some(w => w <= 640),
    `the narrowest breakpoint is ${Math.min(...widths)}px, which is wider than a phone`,
  );
});

test('every class the stylesheet styles is one the app can produce', () => {
  // The other direction — a rule for a class nothing renders is dead weight and
  // usually the leftover of a rename that half happened. This is a substring
  // match, so it is deliberately lenient: it catches a class name that appears
  // nowhere at all, not one that survives only inside a longer word.
  const markup = ['app.html', 'dashboard-app.js', 'theme.js']
    .map(f => readFileSync(join(web, f), 'utf8'))
    .join('\n');
  const styled = new Set([...css.matchAll(/\.([a-z][\w-]*)/g)].map(m => m[1]));
  const orphaned = [...styled].filter(name => !markup.includes(name));
  assert.deepEqual(orphaned.sort(), [], 'these classes are styled but never rendered');
});
