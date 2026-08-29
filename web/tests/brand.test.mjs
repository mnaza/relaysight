// brand.json is the white-label contract: it is fetched at runtime and drives
// the name, palette and locale list. A schema sits next to it and nothing
// checked the file against it.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const web = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = name => JSON.parse(readFileSync(join(web, name), 'utf8'));
const brand = read('brand.json');
const schema = read('brand.schema.json');

test('brand.json carries everything its schema requires', () => {
  for (const key of schema.required ?? []) {
    assert.ok(key in brand, `brand.json is missing required key ${key}`);
  }
});

test('brand.json has no keys the schema does not describe', () => {
  // An unknown key is either a typo that silently does nothing, or a feature
  // whose schema was never updated. Both are worth knowing about.
  const known = new Set(Object.keys(schema.properties ?? {}));
  const unknown = Object.keys(brand).filter(k => !known.has(k));
  assert.deepEqual(unknown, [], 'keys absent from brand.schema.json');
});

test('every advertised locale actually has a file', () => {
  // Offering a language in the picker that has no dictionary drops the user
  // into a page of raw key names.
  const available = new Set(
    readdirSync(join(web, 'locales')).filter(f => f.endsWith('.json')).map(f => f.replace('.json', '')),
  );
  for (const locale of brand.supportedLocales ?? []) {
    assert.ok(available.has(locale), `supportedLocales offers ${locale} with no locales/${locale}.json`);
  }
  assert.ok(
    available.has(brand.defaultLocale),
    `defaultLocale ${brand.defaultLocale} has no dictionary`,
  );
});

test('the default locale is one the brand claims to support', () => {
  assert.ok((brand.supportedLocales ?? []).includes(brand.defaultLocale));
});

test('every theme value matches the type its schema gives it', () => {
  // These are written straight into CSS custom properties, and the browser
  // silently ignores a malformed one — the element just keeps its old value.
  // The schema already says which are colours, which is an enum and which is a
  // number, so check against that rather than assuming they are all colours.
  const props = schema.properties?.theme?.properties ?? {};
  const colour = /^(#[0-9a-fA-F]{3,8}|rgba?\(|hsla?\(|[a-z]+)$/;
  for (const [key, value] of Object.entries(brand.theme ?? {})) {
    const spec = props[key];
    if (!spec) continue;
    if (spec.enum) {
      assert.ok(spec.enum.includes(value), `theme.${key} = ${value} is not one of ${spec.enum}`);
    } else if (spec.type === 'number') {
      assert.equal(typeof value, 'number', `theme.${key} must be a number`);
      assert.ok(value >= (spec.minimum ?? -Infinity), `theme.${key} = ${value} is below its minimum`);
    } else {
      assert.match(String(value), colour, `theme.${key} = ${value} is not a usable CSS colour`);
    }
  }
});

test('the demo fixtures the UI falls back to are well formed', () => {
  // These are what the dashboard renders when the API is unreachable, so a
  // malformed one turns a degraded page into a blank one.
  const fleet = read('demo-fleet.json');
  assert.ok(Array.isArray(fleet.customers), 'demo-fleet.json has no customers array to render');
  assert.ok(fleet.customers.length > 0, 'the offline fallback shows an empty fleet');
  assert.ok(fleet.generated_at, 'demo-fleet.json has no generated_at');
  const plugins = read('demo-plugins.json');
  assert.ok(plugins !== null && typeof plugins === 'object');
});
