// Which files the pages actually load.
//
// Both the i18n scan and the selector check used a hardcoded list, and when
// dashboard.js was split the i18n scan quietly lost a quarter of its keys and
// still passed — the threshold guarding it was too loose to notice. Deriving
// the list from the markup instead means a new module is covered the moment a
// page loads it, and a module that stops being loaded stops being scanned.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, normalize } from 'node:path';

export const web = join(dirname(fileURLToPath(import.meta.url)), '..');

const IMPORT = /from\s+['"](\.[^'"]+)['"]/g;

/** Every local script a page loads, following ES imports transitively. */
export function scriptsFor(page) {
  const html = readFileSync(join(web, page), 'utf8');
  const entries = [...html.matchAll(/<script[^>]+src="([^"]+)"/g)]
    .map(m => m[1])
    .filter(src => !src.startsWith('http') && !src.startsWith('//'))
    .map(src => src.replace(/^\.?\//, ''));

  const seen = new Set();
  const queue = [...entries];
  while (queue.length) {
    const file = normalize(queue.shift());
    if (seen.has(file)) continue;
    seen.add(file);
    const source = readFileSync(join(web, file), 'utf8');
    for (const m of source.matchAll(IMPORT)) {
      queue.push(join(dirname(file), m[1]));
    }
  }
  return [...seen];
}

export const pages = ['app.html'];

/** Every page plus every script it pulls in, deduplicated. */
export function allSources() {
  const files = new Set(pages);
  for (const page of pages) for (const script of scriptsFor(page)) files.add(script);
  return [...files];
}
